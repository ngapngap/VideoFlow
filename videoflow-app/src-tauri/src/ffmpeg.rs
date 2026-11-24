//! Quản lý thực thi FFmpeg an toàn: tự dò đường dẫn, kiểm tra khả dụng,
//! giới hạn thời gian (timeout), dừng tức thì theo `stop_flag`, và đọc thông số stream
//! để bật fast-path `-c copy` khi các clip đồng nhất.

use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Thời gian chờ mặc định cho từng tác vụ FFmpeg.
pub const FFMPEG_PROBE_TIMEOUT: Duration = Duration::from_secs(20);
pub const FFMPEG_FRAME_TIMEOUT: Duration = Duration::from_secs(60);
pub const FFMPEG_CLIP_TIMEOUT: Duration = Duration::from_secs(180);
pub const FFMPEG_CONCAT_TIMEOUT: Duration = Duration::from_secs(180);

const MAX_CAPTURED_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FfmpegStatus {
    pub available: bool,
    pub resolved_path: Option<String>,
    pub version: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FfmpegOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClipProbeInfo {
    pub has_audio: bool,
    pub video_codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<String>,
    pub audio_codec: Option<String>,
    pub audio_sample_rate: Option<u32>,
}

fn exe_file_name() -> &'static str {
    if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    }
}

/// Dò tìm file thực thi FFmpeg theo thứ tự:
/// 1. Đường dẫn thủ công trong Cài đặt (`override_path`)
/// 2. File `ffmpeg(.exe)` đi kèm cạnh ứng dụng (hoặc trong thư mục `binaries/`)
/// 3. Biến môi trường `PATH`
/// 4. Các đường dẫn cài đặt phổ biến trên Windows (WinGet, Chocolatey, `C:\ffmpeg\bin`)
pub fn resolve_ffmpeg_binary(override_path: &str) -> Result<PathBuf, String> {
    let trimmed = override_path.trim();
    if !trimmed.is_empty() {
        let p = PathBuf::from(trimmed);
        if p.is_file() {
            return Ok(p);
        }
        return Err(format!(
            "Đường dẫn FFmpeg trong Cài đặt không tồn tại: {trimmed}"
        ));
    }

    let name = exe_file_name();

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for candidate in [dir.join(name), dir.join("binaries").join(name)] {
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
    }

    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    #[cfg(windows)]
    {
        let mut common: Vec<PathBuf> = vec![
            PathBuf::from(r"C:\ffmpeg\bin\ffmpeg.exe"),
            PathBuf::from(r"C:\ProgramData\chocolatey\bin\ffmpeg.exe"),
        ];
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            common.push(
                PathBuf::from(&local)
                    .join("Microsoft")
                    .join("WinGet")
                    .join("Links")
                    .join("ffmpeg.exe"),
            );
        }
        if let Ok(profile) = std::env::var("USERPROFILE") {
            common.push(
                PathBuf::from(&profile)
                    .join("scoop")
                    .join("shims")
                    .join("ffmpeg.exe"),
            );
        }
        for candidate in common {
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    Err(
        "Chưa tìm thấy FFmpeg trên máy. Hãy cài FFmpeg (vd: chạy `winget install Gyan.FFmpeg` rồi mở lại app) hoặc chọn đường dẫn `ffmpeg.exe` trong Cài đặt → Lịch chạy & Tự động."
            .into(),
    )
}

fn read_bounded<R: Read>(mut reader: R) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let remaining = MAX_CAPTURED_BYTES.saturating_sub(buf.len());
                if remaining > 0 {
                    let take = n.min(remaining);
                    buf.extend_from_slice(&chunk[..take]);
                }
            }
            Err(_) => break,
        }
    }
    buf
}

fn kill_child_process(child: &mut Child) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let pid = child.id().to_string();
        let _ = Command::new("taskkill")
            .args(["/PID", &pid, "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Chạy tiến trình con (FFmpeg) với giới hạn thời gian và hỗ trợ hủy ngay theo `stop_flag`.
/// Đọc stdout/stderr trên luồng riêng để tránh tắc nghẽn pipe khi FFmpeg xuất log dài.
pub fn run_command_with_timeout(
    binary: &Path,
    args: &[String],
    timeout: Duration,
    stop_flag: Option<&AtomicBool>,
) -> Result<FfmpegOutput, String> {
    if stop_flag.is_some_and(|f| f.load(Ordering::SeqCst)) {
        return Err("STOPPED: Đã dừng thao tác FFmpeg theo yêu cầu người dùng.".into());
    }

    let mut cmd = Command::new(binary);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Không thể thực thi lệnh FFmpeg ({}): {e}", binary.display()))?;

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let out_handle = std::thread::spawn(move || stdout_pipe.map(read_bounded).unwrap_or_default());
    let err_handle = std::thread::spawn(move || stderr_pipe.map(read_bounded).unwrap_or_default());

    let started = Instant::now();
    let status = loop {
        if stop_flag.is_some_and(|f| f.load(Ordering::SeqCst)) {
            kill_child_process(&mut child);
            let _ = out_handle.join();
            let _ = err_handle.join();
            return Err("STOPPED: Đã dừng thao tác FFmpeg theo yêu cầu người dùng.".into());
        }
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if started.elapsed() >= timeout {
                    kill_child_process(&mut child);
                    let _ = out_handle.join();
                    let _ = err_handle.join();
                    return Err(format!(
                        "FFmpeg vượt quá thời gian chờ ({} giây) và đã bị dừng — có thể file video nguồn bị hỏng.",
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(40));
            }
            Err(e) => {
                kill_child_process(&mut child);
                let _ = out_handle.join();
                let _ = err_handle.join();
                return Err(format!("Lỗi theo dõi tiến trình FFmpeg: {e}"));
            }
        }
    };

    let stdout = out_handle.join().unwrap_or_default();
    let stderr = err_handle.join().unwrap_or_default();

    Ok(FfmpegOutput {
        status,
        stdout,
        stderr,
    })
}

/// Kiểm tra nhanh trạng thái FFmpeg để hiển thị trong Cài đặt và cảnh báo trước khi chạy.
pub fn check_ffmpeg_status(override_path: &str) -> FfmpegStatus {
    let bin = match resolve_ffmpeg_binary(override_path) {
        Ok(p) => p,
        Err(e) => {
            return FfmpegStatus {
                available: false,
                resolved_path: None,
                version: None,
                error: Some(e),
            };
        }
    };

    let args = vec!["-version".to_string()];
    match run_command_with_timeout(&bin, &args, Duration::from_secs(5), None) {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let first_line = stdout.lines().next().unwrap_or("").trim().to_string();
            let fallback = if first_line.is_empty() {
                None
            } else {
                Some(first_line.clone())
            };
            let version = first_line
                .strip_prefix("ffmpeg version ")
                .and_then(|rest| rest.split_whitespace().next())
                .map(|v| v.to_string())
                .or(fallback);
            FfmpegStatus {
                available: true,
                resolved_path: Some(bin.to_string_lossy().to_string()),
                version,
                error: None,
            }
        }
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            FfmpegStatus {
                available: false,
                resolved_path: Some(bin.to_string_lossy().to_string()),
                version: None,
                error: Some(if err.is_empty() {
                    format!("FFmpeg trả về mã lỗi {:?}", out.status.code())
                } else {
                    err
                }),
            }
        }
        Err(e) => FfmpegStatus {
            available: false,
            resolved_path: Some(bin.to_string_lossy().to_string()),
            version: None,
            error: Some(e),
        },
    }
}

/// Phân tích log `ffmpeg -hide_banner -i <file>` để lấy thông số stream video/audio.
pub fn parse_probe_stderr(stderr: &str) -> ClipProbeInfo {
    let mut info = ClipProbeInfo::default();
    for line in stderr.lines() {
        if !line.contains("Stream #") {
            continue;
        }
        if let Some((_, after)) = line.split_once("Video:") {
            let parts: Vec<&str> = after.split(',').map(|s| s.trim()).collect();
            if let Some(first) = parts.first() {
                if let Some(codec) = first.split_whitespace().next() {
                    if !codec.is_empty() {
                        info.video_codec = Some(codec.to_lowercase());
                    }
                }
            }
            for part in &parts[1..] {
                // Tìm độ phân giải dạng "720x1280" hoặc "1280x720 [SAR 1:1 DAR 16:9]"
                if info.width.is_none() {
                    if let Some(token) = part.split_whitespace().next() {
                        if let Some((ws, hs)) = token.split_once('x') {
                            if let (Ok(w), Ok(h)) = (ws.parse::<u32>(), hs.parse::<u32>()) {
                                if w >= 16 && h >= 16 {
                                    info.width = Some(w);
                                    info.height = Some(h);
                                }
                            }
                        }
                    }
                }
                if info.fps.is_none() && part.ends_with(" fps") {
                    if let Some(val) = part.strip_suffix(" fps") {
                        let clean = val.trim();
                        if !clean.is_empty() {
                            info.fps = Some(clean.to_string());
                        }
                    }
                }
            }
        } else if let Some((_, after)) = line.split_once("Audio:") {
            info.has_audio = true;
            let parts: Vec<&str> = after.split(',').map(|s| s.trim()).collect();
            if let Some(first) = parts.first() {
                if let Some(codec) = first.split_whitespace().next() {
                    if !codec.is_empty() {
                        info.audio_codec = Some(codec.to_lowercase());
                    }
                }
            }
            for part in &parts[1..] {
                if info.audio_sample_rate.is_none() && part.ends_with(" Hz") {
                    if let Some(val) = part.strip_suffix(" Hz") {
                        if let Ok(sr) = val.trim().parse::<u32>() {
                            info.audio_sample_rate = Some(sr);
                        }
                    }
                }
            }
        }
    }
    info
}

/// Kiểm tra xem danh sách clip đã đồng nhất hoàn toàn để có thể ghép nhanh bằng `-c copy` hay không.
pub fn can_fast_stream_copy(infos: &[ClipProbeInfo]) -> bool {
    let Some(first) = infos.first() else {
        return false;
    };
    if infos.len() < 2 {
        return false;
    }
    if first.video_codec.is_none() || first.width.is_none() || first.height.is_none() {
        return false;
    }
    infos.iter().skip(1).all(|item| {
        item.video_codec == first.video_codec
            && item.width == first.width
            && item.height == first.height
            && item.fps == first.fps
            && item.has_audio == first.has_audio
            && (!first.has_audio
                || (item.audio_codec == first.audio_codec
                    && item.audio_sample_rate == first.audio_sample_rate))
    })
}

/// Dò thông số stream của 1 file video qua FFmpeg với timeout và hỗ trợ dừng.
pub fn probe_clip_info(
    binary: &Path,
    video_path: &Path,
    timeout: Duration,
    stop_flag: Option<&AtomicBool>,
) -> Result<ClipProbeInfo, String> {
    if !video_path.is_file() {
        return Err(format!("File video không tồn tại: {}", video_path.display()));
    }
    let args = vec![
        "-hide_banner".to_string(),
        "-i".to_string(),
        video_path.to_string_lossy().to_string(),
    ];
    let out = run_command_with_timeout(binary, &args, timeout, stop_flag)?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    Ok(parse_probe_stderr(&stderr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_probe_stderr_and_fast_copy_compatibility() {
        let sample_a = r#"
Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'a.mp4':
  Duration: 00:00:08.00, start: 0.000000, bitrate: 1500 kb/s
  Stream #0:0[0x1](und): Video: h264 (High) (avc1 / 0x31637661), yuv420p(progressive), 720x1280 [SAR 1:1 DAR 9:16], 1400 kb/s, 30 fps, 30 tbr, 15360 tbn (default)
  Stream #0:1[0x2](und): Audio: aac (LC) (mp4a / 0x6134706D), 44100 Hz, stereo, fltp, 128 kb/s (default)
"#;
        let sample_b = r#"
  Stream #0:0: Video: h264 (Main), yuv420p, 720x1280, 30 fps
  Stream #0:1: Audio: aac (LC), 44100 Hz, stereo
"#;
        let sample_diff_res = r#"
  Stream #0:0: Video: h264 (Main), yuv420p, 1080x1920, 30 fps
  Stream #0:1: Audio: aac (LC), 44100 Hz, stereo
"#;
        let sample_no_audio = r#"
  Stream #0:0: Video: h264 (Main), yuv420p, 720x1280, 30 fps
"#;

        let a = parse_probe_stderr(sample_a);
        assert_eq!(a.video_codec.as_deref(), Some("h264"));
        assert_eq!(a.width, Some(720));
        assert_eq!(a.height, Some(1280));
        assert_eq!(a.fps.as_deref(), Some("30"));
        assert!(a.has_audio);
        assert_eq!(a.audio_codec.as_deref(), Some("aac"));
        assert_eq!(a.audio_sample_rate, Some(44100));

        let b = parse_probe_stderr(sample_b);
        assert!(can_fast_stream_copy(&[a.clone(), b]));

        let diff_res = parse_probe_stderr(sample_diff_res);
        assert!(!can_fast_stream_copy(&[a.clone(), diff_res]));

        let no_aud = parse_probe_stderr(sample_no_audio);
        assert!(!no_aud.has_audio);
        assert!(!can_fast_stream_copy(&[a, no_aud.clone()]));
        assert!(can_fast_stream_copy(&[no_aud.clone(), no_aud]));
    }

    #[test]
    fn test_run_command_respects_timeout_and_stop_flag() {
        let flag = AtomicBool::new(true);
        let dummy = Path::new(if cfg!(windows) { "cmd.exe" } else { "sh" });
        let stopped = run_command_with_timeout(dummy, &[], Duration::from_secs(5), Some(&flag));
        assert!(stopped.unwrap_err().starts_with("STOPPED:"));

        #[cfg(windows)]
        let (bin, args) = (
            PathBuf::from("cmd.exe"),
            vec!["/C".to_string(), "ping -n 6 127.0.0.1 >NUL".to_string()],
        );
        #[cfg(not(windows))]
        let (bin, args) = (
            PathBuf::from("sh"),
            vec!["-c".to_string(), "sleep 5".to_string()],
        );

        let t0 = Instant::now();
        let err = run_command_with_timeout(&bin, &args, Duration::from_millis(200), None)
            .expect_err("must time out");
        assert!(err.contains("vượt quá thời gian chờ"), "unexpected err: {err}");
        assert!(
            t0.elapsed() < Duration::from_secs(3),
            "timeout must kill child quickly"
        );
    }

    #[test]
    fn test_resolve_ffmpeg_override_missing_returns_clear_error() {
        let err = resolve_ffmpeg_binary("/definitely/missing/ffmpeg_nonexistent_bin")
            .expect_err("missing override must fail");
        assert!(err.contains("không tồn tại"));
    }
}
