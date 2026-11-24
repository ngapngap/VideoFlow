//! Tự động ghép nối video sau batch bằng FFmpeg (P2-2).
//! Hỗ trợ:
//! - Kiểm tra trước FFmpeg (tự dò PATH / đường dẫn tùy chọn).
//! - Fast-path `-c copy` khi toàn bộ clip đã đồng nhất codec, độ phân giải, FPS và âm thanh.
//! - Fallback chuẩn hóa `libx264` + bù âm thanh câm nếu clip không đồng nhất hoặc fast-path lỗi.
//! - Timeout cho mọi tiến trình FFmpeg và hỗ trợ hủy tức thì qua `stop_flag`.

use super::{
    image_counts, latest_job_for_csv_row, load_current_media_map, load_latest_jobs_map,
    map_lookup_legacy, row_key_low,
};
use crate::ffmpeg::{
    can_fast_stream_copy, probe_clip_info, resolve_ffmpeg_binary, run_command_with_timeout,
    ClipProbeInfo, FFMPEG_CLIP_TIMEOUT, FFMPEG_CONCAT_TIMEOUT, FFMPEG_PROBE_TIMEOUT,
};
use crate::prompts_csv;
use crate::settings::Settings;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

// ━━ P2-2: Tự động ghép nối video sau batch (Auto-Merge FFmpeg) ━━

/// Thu thập danh sách các file video thành phẩm theo đúng thứ tự tuần tự của các dòng trong prompts.csv.
pub fn collect_batch_video_paths(csv_path: &Path, video_out_dir: &Path) -> Result<Vec<PathBuf>, String> {
    collect_batch_video_paths_filtered(csv_path, video_out_dir, None)
}

/// Thu thập danh sách các file video thành phẩm theo đúng thứ tự dòng CSV (có hỗ trợ lọc tập dòng nếu cần).
pub fn collect_batch_video_paths_filtered(
    csv_path: &Path,
    video_out_dir: &Path,
    filter_keys: Option<&std::collections::HashSet<String>>,
) -> Result<Vec<PathBuf>, String> {
    if !csv_path.is_file() {
        return Err(format!("File CSV không tồn tại: {}", csv_path.display()));
    }
    let rows = prompts_csv::read_all(csv_path)
        .map_err(|e| format!("Không đọc được prompts.csv: {e}"))?;

    let mut videos = Vec::new();
    for row in rows {
        if row.is_example() || row.is_empty_content() {
            continue;
        }
        let key = prompts_csv::row_key_of(&row.anh_tham_chieu, &row.promt_tao_image, &row.promt_tao_video);
        if let Some(keys) = filter_keys {
            if !keys.contains(&key) && !keys.contains(&row.anh_tham_chieu) {
                continue;
            }
        } else if row.trang_thai.trim().eq_ignore_ascii_case("Bỏ qua") {
            continue;
        }
        let vname = row.video_da_tao.trim();
        if !vname.is_empty() {
            let vpath = video_out_dir.join(vname);
            if vpath.is_file() {
                videos.push(vpath);
            }
        }
    }
    Ok(videos)
}

/// Chuẩn hóa tên dự án an toàn theo chuẩn hệ thống file Windows NTFS (loại bỏ < > : " / \ | ? * và ký tự điều khiển).
pub fn sanitize_ntfs_filename(raw: &str) -> String {
    let forbidden = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
    let sanitized: String = raw
        .chars()
        .map(|c| {
            if c.is_control() || forbidden.contains(&c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = sanitized.trim().trim_matches('.').trim();
    trimmed.to_string()
}

/// Tạo tên file tổng hợp đầu ra: `<project_name>_merged_<timestamp>.mp4` (hoặc `merged_<timestamp>.mp4` nếu tên dự án rỗng).
pub fn generate_merged_output_filename(project_name: &str, timestamp: &str) -> String {
    let clean_name = sanitize_ntfs_filename(project_name);
    if clean_name.is_empty() {
        format!("merged_{timestamp}.mp4")
    } else {
        format!("{clean_name}_merged_{timestamp}.mp4")
    }
}

/// Kiểm tra xem file video có chứa luồng âm thanh hay không bằng FFmpeg (có timeout & tự dò đường dẫn).
pub fn probe_has_audio_ffmpeg(video_path: &Path) -> Result<bool, String> {
    if !video_path.is_file() {
        return Err(format!("File video không tồn tại: {}", video_path.display()));
    }
    let bin = resolve_ffmpeg_binary("")?;
    let info = probe_clip_info(&bin, video_path, FFMPEG_PROBE_TIMEOUT, None)?;
    Ok(info.has_audio)
}

/// Tách độ phân giải video từ chuỗi cấu hình `video_size` (mặc định 720x1280).
pub fn parse_video_dimensions(size_str: &str) -> (u32, u32) {
    let parts: Vec<&str> = size_str.split(['x', 'X', ':']).collect();
    if parts.len() == 2 {
        if let (Ok(w), Ok(h)) = (parts[0].trim().parse::<u32>(), parts[1].trim().parse::<u32>()) {
            if w > 0 && h > 0 {
                return (w, h);
            }
        }
    }
    (720, 1280)
}

/// Xây dựng tham số dòng lệnh FFmpeg để chuẩn hóa một clip:
/// - Chuẩn hóa khung hình về cùng tỷ lệ & độ phân giải bằng filter scale & pad.
/// - Đồng bộ FPS (30fps).
/// - Nếu clip thiếu âm thanh: bù track âm thanh câm bằng `anullsrc=channel_layout=stereo:sample_rate=44100` với `-shortest`.
pub fn build_normalize_clip_args(
    input_path: &Path,
    output_path: &Path,
    has_audio: bool,
    width: u32,
    height: u32,
    fps: u32,
) -> Vec<String> {
    let even_w = (width & !1).max(2);
    let even_h = (height & !1).max(2);

    let mut args = vec![
        "-y".into(),
        "-hide_banner".into(),
        "-i".into(),
        input_path.to_string_lossy().to_string(),
    ];

    if !has_audio {
        args.push("-f".into());
        args.push("lavfi".into());
        args.push("-i".into());
        args.push("anullsrc=channel_layout=stereo:sample_rate=44100".into());
        args.push("-map".into());
        args.push("0:v:0".into());
        args.push("-map".into());
        args.push("1:a:0".into());
    }

    let vf = format!(
        "scale={even_w}:{even_h}:force_original_aspect_ratio=decrease,pad={even_w}:{even_h}:(ow-iw)/2:(oh-ih)/2,setsar=1,fps={fps}"
    );
    args.push("-vf".into());
    args.push(vf);

    args.push("-c:v".into());
    args.push("libx264".into());
    args.push("-pix_fmt".into());
    args.push("yuv420p".into());

    args.push("-c:a".into());
    args.push("aac".into());
    args.push("-b:a".into());
    args.push("128k".into());
    args.push("-ar".into());
    args.push("44100".into());
    args.push("-ac".into());
    args.push("2".into());

    if !has_audio {
        args.push("-shortest".into());
    }

    args.push(output_path.to_string_lossy().to_string());
    args
}

/// Xây dựng tham số FFmpeg concat demuxer ghép nối nhanh các clip (stream copy không mã hóa lại).
pub fn build_concat_demuxer_args(list_path: &Path, output_path: &Path) -> Vec<String> {
    vec![
        "-y".into(),
        "-hide_banner".into(),
        "-f".into(),
        "concat".into(),
        "-safe".into(),
        "0".into(),
        "-i".into(),
        list_path.to_string_lossy().to_string(),
        "-c".into(),
        "copy".into(),
        output_path.to_string_lossy().to_string(),
    ]
}

/// Xây dựng tham số FFmpeg filter_complex concat thay thế khi ghép nối luồng trực tiếp.
pub fn build_filter_complex_concat_args(
    inputs: &[PathBuf],
    output_path: &Path,
    input_has_audio: &[bool],
    width: u32,
    height: u32,
    fps: u32,
) -> Vec<String> {
    let mut args = vec!["-y".into(), "-hide_banner".into()];
    for inp in inputs {
        args.push("-i".into());
        args.push(inp.to_string_lossy().to_string());
    }

    let count = inputs.len();
    let mut filter_parts = Vec::new();
    let mut concat_inputs = String::new();

    for i in 0..count {
        let v_label = format!("v{i}");
        let a_label = format!("a{i}");
        filter_parts.push(format!(
            "[{i}:v]scale={width}:{height}:force_original_aspect_ratio=decrease,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2,setsar=1,fps={fps}[{v_label}]"
        ));
        let has_a = input_has_audio.get(i).copied().unwrap_or(true);
        if has_a {
            filter_parts.push(format!("[{i}:a]aformat=sample_rates=44100:channel_layouts=stereo[{a_label}]"));
        } else {
            filter_parts.push(format!("anullsrc=channel_layout=stereo:sample_rate=44100,atrim=duration=10[silent{i}];[silent{i}]aformat=sample_rates=44100:channel_layouts=stereo[{a_label}]"));
        }
        concat_inputs.push_str(&format!("[{v_label}][{a_label}]"));
    }

    filter_parts.push(format!("{concat_inputs}concat=n={count}:v=1:a=1[outv][outa]"));
    let full_filter = filter_parts.join(";");

    args.push("-filter_complex".into());
    args.push(full_filter);
    args.push("-map".into());
    args.push("[outv]".into());
    args.push("-map".into());
    args.push("[outa]".into());
    args.push("-c:v".into());
    args.push("libx264".into());
    args.push("-pix_fmt".into());
    args.push("yuv420p".into());
    args.push("-c:a".into());
    args.push("aac".into());
    args.push(output_path.to_string_lossy().to_string());

    args
}

fn write_concat_list(list_file: &Path, paths: &[PathBuf]) -> Result<(), String> {
    let mut list_content = String::new();
    for p in paths {
        let abs = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let raw = abs.to_string_lossy();
        let stripped = raw.strip_prefix(r"\\?\").unwrap_or(&raw);
        let safe_path = stripped.replace('\\', "/").replace('\'', "'\\''");
        list_content.push_str(&format!("file '{safe_path}'\n"));
    }
    std::fs::write(list_file, list_content.as_bytes())
        .map_err(|e| format!("Không thể ghi file concat_list.txt: {e}"))
}

fn commit_merged_output(tmp_output: &Path, final_output: &Path) -> Result<(), String> {
    let len = std::fs::metadata(tmp_output).map(|m| m.len()).unwrap_or(0);
    if len == 0 {
        return Err("File video sau ghép rỗng (0 bytes)".into());
    }
    if let Err(e) = std::fs::rename(tmp_output, final_output) {
        if let Err(copy_err) = std::fs::copy(tmp_output, final_output) {
            let _ = std::fs::remove_file(final_output);
            return Err(format!("Không thể lưu file video sau ghép: {e} ({copy_err})"));
        }
    }
    Ok(())
}

/// Thực hiện ghép nối tự động các video thành phẩm của batch bằng FFmpeg (wrapper giữ tương thích).
pub fn auto_merge_batch(
    project_path: &Path,
    run_id: Option<i64>,
    conn: Option<&Connection>,
    cfg: &Settings,
) -> Result<Option<PathBuf>, String> {
    auto_merge_batch_with_stop(project_path, run_id, conn, cfg, None)
}

/// Thực hiện ghép nối tự động các video thành phẩm của batch bằng FFmpeg có timeout và hỗ trợ dừng (`stop_flag`).
/// Ưu tiên ghép nhanh `-c copy` nếu mọi clip đồng nhất codec/độ phân giải/FPS/audio, chỉ chuẩn hóa `libx264` làm fallback.
pub fn auto_merge_batch_with_stop(
    project_path: &Path,
    run_id: Option<i64>,
    conn: Option<&Connection>,
    cfg: &Settings,
    stop_flag: Option<&AtomicBool>,
) -> Result<Option<PathBuf>, String> {
    if stop_flag.is_some_and(|f| f.load(Ordering::SeqCst)) {
        return Ok(None);
    }

    let video_out = project_path.join("video_out");
    let csv_path = project_path.join("prompts.csv");

    if !video_out.is_dir() || !csv_path.is_file() {
        return Ok(None);
    }

    let video_paths = if let Some(c) = conn {
        let rows = prompts_csv::read_all(&csv_path)
            .map_err(|e| format!("Không đọc được prompts.csv: {e}"))?;
        let latest_map = load_latest_jobs_map(c);
        let media_map = load_current_media_map(c);
        let img_counts = image_counts(&rows);
        let mut paths = Vec::new();
        for row in &rows {
            if row.is_example() || row.is_empty_content() {
                continue;
            }
            if row.trang_thai.trim().eq_ignore_ascii_case("Bỏ qua") {
                continue;
            }
            let prev = latest_job_for_csv_row(c, row, &latest_map, &img_counts);
            if prev.as_ref().map(|j| j.status.as_str()) == Some("bo_qua") {
                continue;
            }
            let key_low = row_key_low(row);
            let mut vname = row.video_da_tao.trim().to_string();
            if vname.is_empty() || !video_out.join(&vname).is_file() {
                let db_vid = media_map
                    .get(&key_low)
                    .or_else(|| prev.as_ref().and_then(|p| media_map.get(&p.row_key.to_lowercase())))
                    .or_else(|| map_lookup_legacy(&media_map, &rows, row))
                    .and_then(|m| m.video_file.as_deref());
                if let Some(vf) = db_vid {
                    vname = vf.trim().to_string();
                }
            }
            if !vname.is_empty() {
                let vpath = video_out.join(&vname);
                if vpath.is_file() {
                    paths.push(vpath);
                }
            }
        }
        paths
    } else {
        collect_batch_video_paths(&csv_path, &video_out)?
    };

    if video_paths.len() < 2 {
        if let Some(c) = conn {
            crate::settings::log(
                c,
                "info",
                run_id,
                None,
                Some("auto_merge"),
                &format!(
                    "Số lượng video thành phẩm ({}) ít hơn 2 clip, bỏ qua tự động ghép nối.",
                    video_paths.len()
                ),
            );
        }
        return Ok(None);
    }

    let ffmpeg_bin = resolve_ffmpeg_binary(&cfg.ffmpeg_path)?;

    let project_name = crate::project::project_display_name(project_path);
    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
    let out_filename = generate_merged_output_filename(&project_name, &timestamp);
    let final_output = video_out.join(&out_filename);

    let cache_dir = project_path
        .join(".cache")
        .join(format!("merge_{}_{}", timestamp, std::process::id()));
    std::fs::create_dir_all(&cache_dir)
        .map_err(|e| format!("Không tạo được thư mục tạm cho ghép nối video: {e}"))?;

    struct CacheCleanup(PathBuf);
    impl Drop for CacheCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = CacheCleanup(cache_dir.clone());

    // Dò thông số stream từng clip để kiểm tra khả năng ghép nhanh không mã hóa lại (`-c copy`).
    let mut clip_infos: Vec<ClipProbeInfo> = Vec::with_capacity(video_paths.len());
    for vpath in &video_paths {
        if stop_flag.is_some_and(|f| f.load(Ordering::SeqCst)) {
            return Err("STOPPED: Đã dừng ghép nối video theo yêu cầu người dùng.".into());
        }
        let info = probe_clip_info(&ffmpeg_bin, vpath, FFMPEG_PROBE_TIMEOUT, stop_flag)
            .unwrap_or_default();
        clip_infos.push(info);
    }

    // Fast-path: nếu toàn bộ clip đã đồng nhất codec/độ phân giải/FPS/âm thanh -> ghép thẳng bằng `-c copy`!
    if can_fast_stream_copy(&clip_infos) {
        let fast_list = cache_dir.join("concat_fast_list.txt");
        let fast_tmp = cache_dir.join("merged_fast_tmp.mp4");
        if write_concat_list(&fast_list, &video_paths).is_ok() {
            let fast_args = build_concat_demuxer_args(&fast_list, &fast_tmp);
            match run_command_with_timeout(&ffmpeg_bin, &fast_args, FFMPEG_CONCAT_TIMEOUT, stop_flag)
            {
                Ok(out) if out.status.success() && fast_tmp.is_file() => {
                    if commit_merged_output(&fast_tmp, &final_output).is_ok() {
                        if let Some(c) = conn {
                            crate::settings::log(
                                c,
                                "info",
                                run_id,
                                None,
                                Some("auto_merge"),
                                &format!(
                                    "Đã ghép nhanh (-c copy) {} video thành công -> {}",
                                    video_paths.len(),
                                    out_filename
                                ),
                            );
                        }
                        return Ok(Some(final_output));
                    }
                }
                Err(e) if e.starts_with("STOPPED:") => return Err(e),
                _ => {
                    // Fallback về chuẩn hóa từng clip nếu stream-copy gặp lỗi container/timestamp
                    let _ = std::fs::remove_file(&fast_tmp);
                }
            }
        }
    }

    let (w, h) = parse_video_dimensions(&cfg.video_size);
    let fps = 30;

    // Fallback bước 1: Chuẩn hóa từng clip (độ phân giải, fps, âm thanh câm nếu thiếu)
    let mut normalized_clips = Vec::with_capacity(video_paths.len());
    for (i, vpath) in video_paths.iter().enumerate() {
        if stop_flag.is_some_and(|f| f.load(Ordering::SeqCst)) {
            return Err("STOPPED: Đã dừng ghép nối video theo yêu cầu người dùng.".into());
        }
        let norm_path = cache_dir.join(format!("norm_{i:04}.mp4"));
        let has_audio = clip_infos.get(i).map(|info| info.has_audio).unwrap_or(false);
        let args = build_normalize_clip_args(vpath, &norm_path, has_audio, w, h, fps);

        let output =
            run_command_with_timeout(&ffmpeg_bin, &args, FFMPEG_CLIP_TIMEOUT, stop_flag)?;
        if !output.status.success() || !norm_path.is_file() {
            let err_msg = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "FFmpeg chuẩn hóa clip thất bại cho {}: {}",
                vpath.display(),
                err_msg.trim()
            ));
        }
        normalized_clips.push(norm_path);
    }

    // Fallback bước 2: Ghi file danh sách concat demuxer
    let list_file = cache_dir.join("concat_list.txt");
    write_concat_list(&list_file, &normalized_clips)?;

    // Fallback bước 3: Ghép nối ra file tạm rồi rename nguyên tử sang final_output
    let tmp_output = cache_dir.join("merged_tmp.mp4");
    let concat_args = build_concat_demuxer_args(&list_file, &tmp_output);
    let output =
        run_command_with_timeout(&ffmpeg_bin, &concat_args, FFMPEG_CONCAT_TIMEOUT, stop_flag)?;

    if !output.status.success() || !tmp_output.is_file() {
        let err_msg = String::from_utf8_lossy(&output.stderr);
        return Err(format!("FFmpeg ghép nối thất bại: {}", err_msg.trim()));
    }

    commit_merged_output(&tmp_output, &final_output)?;

    if let Some(c) = conn {
        crate::settings::log(
            c,
            "info",
            run_id,
            None,
            Some("auto_merge"),
            &format!(
                "Đã tự động ghép nối {} video thành công -> {}",
                video_paths.len(),
                out_filename
            ),
        );
    }

    Ok(Some(final_output))
}
