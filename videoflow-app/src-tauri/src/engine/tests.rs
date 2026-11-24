use super::*;
use crate::prompts_csv::Row;

static TEST_IDX: AtomicUsize = AtomicUsize::new(0);

fn make_temp_project(tag: &str) -> std::path::PathBuf {
    let idx = TEST_IDX.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "videoflow_engine_test_{}_{}_{}",
        tag,
        std::process::id(),
        idx
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("input")).unwrap();
    std::fs::create_dir_all(dir.join("image_out")).unwrap();
    std::fs::create_dir_all(dir.join("video_out")).unwrap();
    dir
}

#[test]
fn test_next_version_checks_disk_even_when_state_db_recreated() {
    let dir = make_temp_project("next_ver");
    let conn = crate::state_db::open(&dir).unwrap();
    let img_out = dir.join("image_out");

    std::fs::write(img_out.join("cat_gen.png"), b"v1").unwrap();
    std::fs::write(img_out.join("cat_gen_v2.png"), b"v2").unwrap();

    let ver = next_version(&conn, &img_out, "cat.png", "anh");
    assert_eq!(ver, 3);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_external_prompt_edit_detected_on_completed_video_row_13_4() {
    let dir = make_temp_project("prompt_13_4");
    let img_out = dir.join("image_out");
    std::fs::write(img_out.join("hero_gen.png"), b"png").unwrap();

    let cfg = Settings::default();
    let eng_cfg = EngineConfig::default();

    let mut row = Row {
        row_no: 2,
        anh_tham_chieu: "hero.png".into(),
        promt_tao_image: "prompt anh cu".into(),
        promt_tao_video: "prompt video cu".into(),
        anh_da_tao: "hero_gen.png".into(),
        video_da_tao: "hero_gen.mp4".into(),
        trang_thai: "Đã tạo video".into(),
        ..Default::default()
    };

    let prev = LatestJobInfo {
        id: 1,
        row_key: "hero.png".into(),
        status: "da_tao_video".into(),
        image_status: "xong".into(),
        video_status: "xong".into(),
        image_prompt_hash: Some(sha2hex("prompt anh cu")),
        video_prompt_hash: Some(sha2hex("prompt video cu")),
        attempts_count: 0,
        last_error: None,
        video_task_id: None,
        video_download_url: None,
    };

    assert!(plan_row(&row, Some(&prev), &cfg, &eng_cfg, &img_out, Some("hero_gen.png"), 1).is_none());

    row.promt_tao_video = "prompt video MOI".into();
    let p_vid = plan_row(&row, Some(&prev), &cfg, &eng_cfg, &img_out, Some("hero_gen.png"), 1).unwrap();
    assert_eq!(p_vid.steps, vec![Step::B3]);
    assert!(!p_vid.force_new_image);
    assert!(p_vid.force_new_video);
    assert_eq!(p_vid.carried_attempts, 0);

    row.promt_tao_image = "prompt anh MOI".into();
    let p_img = plan_row(&row, Some(&prev), &cfg, &eng_cfg, &img_out, Some("hero_gen.png"), 1).unwrap();
    assert_eq!(p_img.steps, vec![Step::B2, Step::B3]);
    assert!(p_img.force_new_image);
    assert!(p_img.force_new_video);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_selection_guards_bo_qua_max_attempts_and_rerun_error_rows_unlock() {
    let dir = make_temp_project("guards");
    let img_out = dir.join("image_out");
    let cfg = Settings::default();

    let row = Row {
        row_no: 2,
        anh_tham_chieu: "item.png".into(),
        promt_tao_image: "img".into(),
        promt_tao_video: "vid".into(),
        trang_thai: "Lỗi tạo image".into(),
        ..Default::default()
    };

    let single_or_batch_cfg = EngineConfig {
        kind: "rieng",
        row_keys: vec!["item.png".into()],
        force_step: None,
        reset_error_attempts: false,
    };

    let prev_skipped = LatestJobInfo {
        id: 1,
        row_key: "item.png".into(),
        status: "bo_qua".into(),
        image_status: "chua_chay".into(),
        video_status: "chua_chay".into(),
        image_prompt_hash: Some(sha2hex("img")),
        video_prompt_hash: Some(sha2hex("vid")),
        attempts_count: 0,
        last_error: None,
        video_task_id: None,
        video_download_url: None,
    };
    assert!(plan_row(&row, Some(&prev_skipped), &cfg, &single_or_batch_cfg, &img_out, None, 1).is_none());

    let prev_maxed = LatestJobInfo {
        status: "loi_tao_image".into(),
        attempts_count: 3,
        ..prev_skipped.clone()
    };
    // Issue #6 [P0-6]: Bỏ khóa cứng, dòng 3/3 lỗi vẫn được plan để chạy lại (không bị chặn)
    assert!(plan_row(&row, Some(&prev_maxed), &cfg, &single_or_batch_cfg, &img_out, None, 1).is_some());

    // rerun_error_rows (reset_error_attempts: true) mở khóa các dòng lỗi của lượt trước và reset carried_attempts = 0!
    let rerun_err_cfg = EngineConfig {
        kind: "lam_lai",
        row_keys: vec!["item.png".into()],
        force_step: None,
        reset_error_attempts: true,
    };
    let unlocked = plan_row(&row, Some(&prev_maxed), &cfg, &rerun_err_cfg, &img_out, None, 1)
        .expect("rerun_error_rows must plan error rows");
    assert_eq!(unlocked.carried_attempts, 0);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_video_task_resume_and_b2_redo_stop_preserves_pending_b2() {
    let stop_b2_redo = resolve_step_failure(Step::B2, &ApiFail::stopped("stop"), true, true);
    assert_eq!(
        stop_b2_redo.db_status, "dang_cho",
        "stopping mid-B2 redo must keep dang_cho so next START still executes B2"
    );
    assert_eq!(stop_b2_redo.image_status, "chua_chay");

    let stop_b3 = resolve_step_failure(Step::B3, &ApiFail::stopped("stop"), true, false);
    assert_eq!(stop_b3.db_status, "da_tao_image");
    assert_eq!(stop_b3.image_status, "xong");

    let b3_fail = ApiFail {
        kind: ApiFailKind::TamThoi { retry_after_s: None },
        http_status: Some(502),
        code: "HTTP_502".into(),
        message: "bad gateway".into(),
    };
    let b3_res = resolve_step_failure(Step::B3, &b3_fail, true, false);
    assert_eq!(b3_res.db_status, "loi_tam_thoi");
    assert_eq!(b3_res.image_status, "xong");

    let b3_biz_fail = ApiFail {
        kind: ApiFailKind::NghiepVu,
        http_status: None,
        code: "POLICY_VIOLATION".into(),
        message: "prompt rejected".into(),
    };
    let b3_biz_res = resolve_step_failure(Step::B3, &b3_biz_fail, true, false);
    assert_eq!(b3_biz_res.db_status, "loi_tao_video");
    assert_eq!(b3_biz_res.image_status, "xong");
}

#[test]
fn test_tc44_excel_lock_recovery_syncs_media_files_and_timestamps() {
    let dir = make_temp_project("tc44_sync");
    std::fs::write(dir.join("image_out").join("lock_gen.png"), b"png_data").unwrap();
    std::fs::write(dir.join("video_out").join("lock_gen.mp4"), b"mp4_data").unwrap();

    let csv_path = dir.join("prompts.csv");
    prompts_csv::write_all(
        &csv_path,
        &[Row {
            row_no: 2,
            anh_tham_chieu: "lock.png".into(),
            promt_tao_image: "p_img".into(),
            promt_tao_video: "p_vid".into(),
            trang_thai: "Đang chờ".into(),
            ..Default::default()
        }],
    )
    .unwrap();

    let conn = crate::state_db::open(&dir).unwrap();
    conn.execute(
        "INSERT INTO runs(kind, status, started_at, total_rows) VALUES ('chung', 'hoan_tat', '2026-11-18T00:00:00Z', 1)",
        [],
    )
    .unwrap();
    let rid = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status, image_prompt_hash, video_prompt_hash)
         VALUES (?1, 'lock.png', 2, 'da_tao_video', 'xong', 'xong', ?2, ?3)",
        rusqlite::params![rid, sha2hex("p_img"), sha2hex("p_vid")],
    )
    .unwrap();
    let jid = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
         VALUES (?1, 'lock.png', 'anh', 'lock_gen.png', 'image_out/lock_gen.png', 1, 1, '2026-11-18T08:15:00+07:00')",
        rusqlite::params![jid],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
         VALUES (?1, 'lock.png', 'video', 'lock_gen.mp4', 'video_out/lock_gen.mp4', 1, 1, '2026-11-18T08:20:00+07:00')",
        rusqlite::params![jid],
    )
    .unwrap();

    let synced = sync_csv_if_unlocked(&dir);
    assert_eq!(synced, 1);

    let reread = prompts_csv::read_all(&csv_path).unwrap();
    assert_eq!(reread[0].anh_da_tao, "lock_gen.png");
    assert_eq!(reread[0].video_da_tao, "lock_gen.mp4");
    assert!(!reread[0].ngay_tao_anh.is_empty(), "must sync col G ngay_tao_anh");
    assert!(!reread[0].ngay_tao_video.is_empty(), "must sync col H ngay_tao_video");
    assert_eq!(reread[0].trang_thai, "Đã tạo video");

    let _ = std::fs::remove_dir_all(&dir);
}

// ━━━━━━━━━━━━━━━━━━━━━━ Real Local-HTTP End-to-End Integration Suite ━━━━━━━━━━━━━━━━━━━━━━

struct RawHttpReq {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

struct RawHttpResp {
    status: u16,
    reason: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
}

impl RawHttpResp {
    fn json(status: u16, reason: &'static str, val: serde_json::Value) -> Self {
        Self {
            status,
            reason,
            content_type: "application/json",
            body: serde_json::to_vec(&val).unwrap(),
        }
    }

    fn binary(status: u16, reason: &'static str, content_type: &'static str, body: Vec<u8>) -> Self {
        Self {
            status,
            reason,
            content_type,
            body,
        }
    }
}

fn read_http_request(stream: &mut std::net::TcpStream) -> Option<RawHttpReq> {
    use std::io::Read as _;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        match stream.read(&mut chunk) {
            Ok(0) => return None,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
            }
            Err(_) => return None,
        }
    };

    let head_str = String::from_utf8_lossy(&buf[..header_end - 4]);
    let mut lines = head_str.split("\r\n");
    let req_line = lines.next()?;
    let mut parts = req_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();

    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let mut body = buf[header_end..].to_vec();
    if let Some(cl_str) = headers.get("content-length") {
        if let Ok(cl) = cl_str.parse::<usize>() {
            while body.len() < cl {
                match stream.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => body.extend_from_slice(&chunk[..n]),
                    Err(_) => break,
                }
            }
            body.truncate(cl);
        }
    } else if headers
        .get("transfer-encoding")
        .is_some_and(|te| te.to_ascii_lowercase().contains("chunked"))
    {
        while !body.windows(5).any(|w| w == b"0\r\n\r\n") {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => body.extend_from_slice(&chunk[..n]),
                Err(_) => break,
            }
        }
    }

    Some(RawHttpReq {
        method,
        path,
        headers,
        body,
    })
}

fn spawn_mock_http_server<F>(
    handler: F,
) -> (u16, Arc<AtomicBool>, std::thread::JoinHandle<()>)
where
    F: Fn(RawHttpReq) -> RawHttpResp + Send + Sync + 'static,
{
    use std::io::Write as _;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_bg = shutdown.clone();
    let handler = Arc::new(handler);

    let join = std::thread::spawn(move || {
        while !shutdown_bg.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let handler_req = handler.clone();
                    // Xử lý từng kết nối trên luồng con để kiểm thử thực tế `concurrency = 2..3` song song!
                    std::thread::spawn(move || {
                        if let Some(req) = read_http_request(&mut stream) {
                            let resp = handler_req(req);
                            let head = format!(
                                "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                resp.status,
                                resp.reason,
                                resp.content_type,
                                resp.body.len()
                            );
                            let _ = stream.write_all(head.as_bytes());
                            let _ = stream.write_all(&resp.body);
                            let _ = stream.flush();
                        }
                    });
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });

    (port, shutdown, join)
}

fn sample_png_bytes(r: u8, g: u8, b: u8) -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([r, g, b, 255]));
    let mut out = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
}

fn sample_jpeg_b64(r: u8, g: u8, b: u8) -> String {
    use base64::Engine as _;
    let img = image::RgbImage::from_pixel(4, 4, image::Rgb([r, g, b]));
    let mut out = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Jpeg)
        .unwrap();
    base64::engine::general_purpose::STANDARD.encode(out)
}

fn sample_mp4_bytes(tag: &[u8; 4]) -> Vec<u8> {
    let mut mp4 = vec![0x00, 0x00, 0x00, 0x18];
    mp4.extend_from_slice(b"ftypmp42");
    mp4.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    mp4.extend_from_slice(b"mp42isom");
    mp4.extend_from_slice(tag);
    mp4
}

#[test]
fn test_e2e_http_concurrency_2_origin_boundary_and_v2_prompt_versioning() {
    let dir = make_temp_project("e2e_conc2_v2");
    std::fs::write(dir.join("input").join("shoe.png"), sample_png_bytes(10, 20, 30)).unwrap();
    std::fs::write(dir.join("input").join("hat.png"), sample_png_bytes(40, 50, 60)).unwrap();

    prompts_csv::write_all(
        &dir.join("prompts.csv"),
        &[
            Row {
                row_no: 2,
                anh_tham_chieu: "shoe.png".into(),
                promt_tao_image: "anh giay studio v1".into(),
                promt_tao_video: "video xoay 360 v1".into(),
                trang_thai: "Đang chờ".into(),
                ..Default::default()
            },
            Row {
                row_no: 3,
                anh_tham_chieu: "hat.png".into(),
                promt_tao_image: "anh mu v1".into(),
                promt_tao_video: "video mu v1".into(),
                trang_thai: "Đang chờ".into(),
                ..Default::default()
            },
        ],
    )
    .unwrap();

    let s3_had_auth = Arc::new(AtomicBool::new(false));
    let s3_auth_ref = s3_had_auth.clone();
    let (port_b, stop_b, join_b) = spawn_mock_http_server(move |req| {
        if req.headers.contains_key("authorization") {
            s3_auth_ref.store(true, Ordering::SeqCst);
        }
        RawHttpResp::binary(200, "OK", "video/mp4", sample_mp4_bytes(b"VIDX"))
    });

    let img_had_mime = Arc::new(AtomicBool::new(false));
    let vid_had_idem = Arc::new(AtomicBool::new(false));
    let create_vid_seq = Arc::new(AtomicUsize::new(0));
    let poll_counts: Arc<parking_lot::Mutex<HashMap<String, usize>>> =
        Arc::new(parking_lot::Mutex::new(HashMap::new()));

    let img_mime_ref = img_had_mime.clone();
    let vid_idem_ref = vid_had_idem.clone();
    let create_seq_ref = create_vid_seq.clone();
    let poll_map_ref = poll_counts.clone();

    let (port_a, stop_a, join_a) = spawn_mock_http_server(move |req| {
        if req.method == "POST" && req.path == "/v1/images/edits" {
            let body_lossy = String::from_utf8_lossy(&req.body);
            if body_lossy.to_ascii_lowercase().contains("content-type: image/png") {
                img_mime_ref.store(true, Ordering::SeqCst);
            }
            return RawHttpResp::json(
                200,
                "OK",
                serde_json::json!({ "data": [{ "b64_json": sample_jpeg_b64(80, 160, 240) }] }),
            );
        }

        if req.method == "POST" && req.path == "/v1/videos" {
            if req.headers.contains_key("idempotency-key") {
                vid_idem_ref.store(true, Ordering::SeqCst);
            }
            let n = create_seq_ref.fetch_add(1, Ordering::SeqCst) + 1;
            return RawHttpResp::json(
                201,
                "Created",
                serde_json::json!({ "id": format!("task_conc_{n}") }),
            );
        }

        if req.method == "GET" && req.path.starts_with("/v1/videos/task_conc_") {
            let mut pm = poll_map_ref.lock();
            let count = pm.entry(req.path.clone()).or_insert(0);
            *count += 1;
            let c = *count;
            drop(pm);
            return if c == 1 {
                RawHttpResp::json(
                    200,
                    "OK",
                    serde_json::json!({ "status": "in_progress", "progress": 55 }),
                )
            } else {
                RawHttpResp::json(
                    200,
                    "OK",
                    serde_json::json!({
                        "status": "succeeded",
                        "progress": 100,
                        "video_url": format!("http://127.0.0.1:{port_b}/presigned/out.mp4?X-Amz-Signature=sig")
                    }),
                )
            };
        }

        RawHttpResp::json(404, "Not Found", serde_json::json!({}))
    });

    let api_base = format!("http://127.0.0.1:{port_a}/v1");
    adapters::register_test_api_key(&api_base, "sk-e2e-api-secret");

    let cfg = Settings {
        endpoint_image_base_url: api_base.clone(),
        endpoint_video_base_url: api_base,
        concurrency: 2, // Chạy song song 2 dòng thực tế!
        poll_interval_s: 1,
        poll_max_s: 30,
        backoff_s: [1, 1, 1],
        ..Settings::default()
    };

    let max_seen_active = Arc::new(AtomicUsize::new(0));
    let max_active_ref = max_seen_active.clone();
    let emitter = move |name: &str, val: serde_json::Value| {
        if name == "progress" {
            if let Some(ac) = val.get("active_count").and_then(|v| v.as_u64()) {
                max_active_ref.fetch_max(ac as usize, Ordering::SeqCst);
            }
        }
    };
    let stop_flag = Arc::new(AtomicBool::new(false));

    let out1 = run_with_emitter(&emitter, &cfg, &dir, EngineConfig::default(), &stop_flag)
        .expect("concurrency=2 E2E run must succeed");
    assert_eq!(out1.done, 2);
    assert_eq!(out1.errors, 0);
    assert!(img_had_mime.load(Ordering::SeqCst));
    assert!(vid_had_idem.load(Ordering::SeqCst), "POST /v1/videos must send Idempotency-Key");
    assert!(!s3_had_auth.load(Ordering::SeqCst), "Port B must NOT receive Bearer token");
    assert!(
        max_seen_active.load(Ordering::SeqCst) >= 2,
        "concurrency=2 must execute 2 active rows concurrently"
    );
    assert!(dir.join("video_out").join("shoe_gen.mp4").is_file());
    assert!(dir.join("video_out").join("hat_gen.mp4").is_file());

    // Sửa prompt shoe.png -> bấm STOP ngay trước B2 redo -> kiểm chứng prev_img_hash cũ được giữ nguyên
    // và lần chạy kế tiếp VẪN chạy đủ B2 + B3 sinh _v2! ([P0-2])
    let edited = crate::commands::save_row_prompts_at(
        &dir,
        "shoe.png",
        "anh giay studio v2",
        "video xoay 360 v2",
    )
    .unwrap();
    assert!(edited.row.prompt_modified);

    stop_flag.store(true, Ordering::SeqCst);
    let out_stopped_b2 =
        run_with_emitter(&emitter, &cfg, &dir, EngineConfig::default(), &stop_flag)
            .expect("mid-B2 redo stop must succeed cleanly");
    assert!(out_stopped_b2.stopped);
    assert_eq!(out_stopped_b2.done, 0);
    assert_eq!(out_stopped_b2.errors, 0);
    assert!(
        !dir.join("image_out").join("shoe_gen_v2.png").exists(),
        "shoe_gen_v2.png must not exist when stopped before B2"
    );

    // Bỏ cờ dừng -> chạy lượt kế tiếp phải bắt buộc chạy lại B2 + B3 sinh _v2!
    stop_flag.store(false, Ordering::SeqCst);
    let out2 = run_with_emitter(&emitter, &cfg, &dir, EngineConfig::default(), &stop_flag)
        .expect("v2 prompt edit run");
    assert_eq!(out2.done, 1);
    assert!(dir.join("image_out").join("shoe_gen.png").is_file());
    assert!(dir.join("image_out").join("shoe_gen_v2.png").is_file());
    assert!(dir.join("video_out").join("shoe_gen_v2.mp4").is_file());

    stop_a.store(true, Ordering::SeqCst);
    stop_b.store(true, Ordering::SeqCst);
    let _ = join_a.join();
    let _ = join_b.join();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_e2e_http_resume_quota_attempts_cap_3_of_3_and_reset_to_zero_on_success() {
    let dir = make_temp_project("e2e_resume_cap");
    std::fs::write(dir.join("input").join("bag.png"), sample_png_bytes(50, 60, 70)).unwrap();
    std::fs::write(
        dir.join("image_out").join("bag_gen.png"),
        sample_png_bytes(80, 90, 100),
    )
    .unwrap();

    prompts_csv::write_all(
        &dir.join("prompts.csv"),
        &[Row {
            row_no: 2,
            anh_tham_chieu: "bag.png".into(),
            promt_tao_image: "bag img".into(),
            promt_tao_video: "bag vid".into(),
            anh_da_tao: "bag_gen.png".into(),
            trang_thai: "Đã tạo image".into(),
            ..Default::default()
        }],
    )
    .unwrap();
    // Engine lưu jobs theo KHOÁ DÒNG (tên ảnh + hash prompt), không phải tên ảnh.
    let bag_key = crate::prompts_csv::row_key_of("bag.png", "bag img", "bag vid");

    let phase = Arc::new(AtomicUsize::new(1));
    let post_video_calls = Arc::new(AtomicUsize::new(0));
    let trigger_stop_on_poll = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::new(AtomicBool::new(false));

    let phase_b = phase.clone();
    let (port_b, stop_b, join_b) = spawn_mock_http_server(move |_req| {
        if phase_b.load(Ordering::SeqCst) == 2 {
            RawHttpResp::json(500, "Internal Server Error", serde_json::json!({ "error": "cdn down" }))
        } else {
            RawHttpResp::binary(200, "OK", "video/mp4", sample_mp4_bytes(b"RESM"))
        }
    });

    let phase_a = phase.clone();
    let post_calls_a = post_video_calls.clone();
    let trig_stop_a = trigger_stop_on_poll.clone();
    let stop_flag_a = stop_flag.clone();
    let (port_a, stop_a, join_a) = spawn_mock_http_server(move |req| {
        if req.method == "POST" && req.path == "/v1/videos" {
            let c = post_calls_a.fetch_add(1, Ordering::SeqCst) + 1;
            return RawHttpResp::json(
                200,
                "OK",
                serde_json::json!({ "id": format!("task_quota_{c}") }),
            );
        }
        if req.method == "GET" && req.path.starts_with("/v1/videos/") {
            if trig_stop_a.load(Ordering::SeqCst) {
                stop_flag_a.store(true, Ordering::SeqCst);
                return RawHttpResp::json(
                    200,
                    "OK",
                    serde_json::json!({ "status": "running", "progress": 25 }),
                );
            }
            if phase_a.load(Ordering::SeqCst) == 1 {
                return RawHttpResp::json(
                    200,
                    "OK",
                    serde_json::json!({ "status": "running", "progress": 0 }),
                );
            }
            if phase_a.load(Ordering::SeqCst) == 99 {
                return RawHttpResp::json(
                    502,
                    "Bad Gateway",
                    serde_json::json!({ "error": "gateway down" }),
                );
            }
            return RawHttpResp::json(
                200,
                "OK",
                serde_json::json!({
                    "status": "succeeded",
                    "video_url": format!("http://127.0.0.1:{port_b}/cdn/bag.mp4")
                }),
            );
        }
        RawHttpResp::json(404, "Not Found", serde_json::json!({}))
    });

    let api_base = format!("http://127.0.0.1:{port_a}/v1");
    adapters::register_test_api_key(&api_base, "sk-quota-key");
    let cfg = Settings {
        endpoint_image_base_url: api_base.clone(),
        endpoint_video_base_url: api_base,
        poll_interval_s: 1,
        poll_max_s: 1,
        backoff_s: [1, 1, 1],
        max_attempts: 3,
        ..Settings::default()
    };
    let noop_emit = |_name: &str, _val: serde_json::Value| {};

    // 1) Dừng an toàn mid-poll
    trigger_stop_on_poll.store(true, Ordering::SeqCst);
    let out_stop =
        run_with_emitter(&noop_emit, &cfg, &dir, EngineConfig::default(), &stop_flag).unwrap();
    assert!(out_stop.stopped);
    assert_eq!(out_stop.errors, 0);

    // 2) POLL_TIMEOUT -> transient error: ghi attempts nhưng attempts_count = 0, status = loi_tam_thoi
    trigger_stop_on_poll.store(false, Ordering::SeqCst);
    stop_flag.store(false, Ordering::SeqCst);
    let _ = run_with_emitter(&noop_emit, &cfg, &dir, EngineConfig::default(), &stop_flag).unwrap();
    {
        let conn = crate::state_db::open(&dir).unwrap();
        let job = latest_job_info(&conn, &bag_key).unwrap();
        assert_eq!(job.status, "loi_tam_thoi");
        assert_eq!(job.attempts_count, 0, "transient error must not increment attempts_count");
    }

    // 3) Poll gặp HTTP 502 liên tiếp -> transient error dừng sau local retry, attempts_count = 0
    phase.store(99, Ordering::SeqCst);
    let _ = run_with_emitter(&noop_emit, &cfg, &dir, EngineConfig::default(), &stop_flag).unwrap();
    {
        let conn = crate::state_db::open(&dir).unwrap();
        let job = latest_job_info(&conn, &bag_key).unwrap();
        assert_eq!(job.status, "loi_tam_thoi");
        assert_eq!(job.attempts_count, 0, "transient 502 error must not increment attempts_count");
    }

    // 4) Chạy lại bằng `reset_error_attempts: true` (TC-45 rerun_error_rows) ở Phase 2 (DOWNLOAD_FAIL) rồi Phase 3 (OK)
    phase.store(2, Ordering::SeqCst);
    let rerun_cfg = EngineConfig {
        kind: "lam_lai",
        row_keys: vec!["bag.png".into()],
        force_step: None,
        reset_error_attempts: true,
    };
    let _ = run_with_emitter(&noop_emit, &cfg, &dir, rerun_cfg, &stop_flag).unwrap();
    {
        let conn = crate::state_db::open(&dir).unwrap();
        assert_eq!(
            latest_job_info(&conn, &bag_key).unwrap().attempts_count,
            0,
            "DOWNLOAD_FAIL must not increment generation attempts_count"
        );
    }

    phase.store(3, Ordering::SeqCst);
    let out_ok =
        run_with_emitter(&noop_emit, &cfg, &dir, EngineConfig::default(), &stop_flag).unwrap();
    assert_eq!(out_ok.done, 1);
    assert_eq!(
        post_video_calls.load(Ordering::SeqCst),
        1,
        "POST /v1/videos must still be called ONLY ONCE across all 5 runs!"
    );
    {
        let conn = crate::state_db::open(&dir).unwrap();
        assert_eq!(
            latest_job_info(&conn, &bag_key).unwrap().attempts_count,
            0,
            "[P1-1]: attempts_count must reset to 0 on row completion"
        );
    }

    stop_a.store(true, Ordering::SeqCst);
    stop_b.store(true, Ordering::SeqCst);
    let _ = join_a.join();
    let _ = join_b.join();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_p0_6_is_transient_error_classification() {
    let t_502 = ApiFail {
        kind: ApiFailKind::TamThoi { retry_after_s: None },
        http_status: Some(502),
        code: "502".into(),
        message: "Bad gateway".into(),
    };
    assert!(is_transient_error(&t_502));

    let t_429 = ApiFail {
        kind: ApiFailKind::TamThoi { retry_after_s: Some(10) },
        http_status: Some(429),
        code: "RATE_LIMIT".into(),
        message: "Too many requests".into(),
    };
    assert!(is_transient_error(&t_429));

    let t_cdp = ApiFail {
        kind: ApiFailKind::TamThoi { retry_after_s: None },
        http_status: None,
        code: "CDP_CLOSED".into(),
        message: "cdp connection closed".into(),
    };
    assert!(is_transient_error(&t_cdp));

    let t_timeout = ApiFail {
        kind: ApiFailKind::TamThoi { retry_after_s: None },
        http_status: None,
        code: "POLL_TIMEOUT".into(),
        message: "poll timed out".into(),
    };
    assert!(is_transient_error(&t_timeout));

    let business_refused = ApiFail {
        kind: ApiFailKind::NghiepVu,
        http_status: Some(502),
        code: "UPSTREAM_REFUSED".into(),
        message: "Prompt refused by safety policy".into(),
    };
    assert!(!is_transient_error(&business_refused));

    let quota_err = ApiFail {
        kind: ApiFailKind::HetQuota,
        http_status: Some(429),
        code: "QUOTA_EXHAUSTED".into(),
        message: "Quota out".into(),
    };
    assert!(!is_transient_error(&quota_err));
}

#[test]
fn test_p0_6_resolve_step_failure_sets_loi_tam_thoi() {
    let transient_fail = ApiFail {
        kind: ApiFailKind::TamThoi { retry_after_s: None },
        http_status: Some(502),
        code: "502".into(),
        message: "Gateway Error".into(),
    };
    let res_b2 = resolve_step_failure(Step::B2, &transient_fail, false, false);
    assert_eq!(res_b2.db_status, "loi_tam_thoi");
    assert_eq!(res_b2.csv_label, "Lỗi tạm thời");

    let res_b3 = resolve_step_failure(Step::B3, &transient_fail, true, false);
    assert_eq!(res_b3.db_status, "loi_tam_thoi");
    assert_eq!(res_b3.csv_label, "Lỗi tạm thời");

    let business_fail = ApiFail {
        kind: ApiFailKind::NghiepVu,
        http_status: None,
        code: "UPSTREAM_REFUSED".into(),
        message: "Refused".into(),
    };
    let res_biz_b2 = resolve_step_failure(Step::B2, &business_fail, false, false);
    assert_eq!(res_biz_b2.db_status, "loi_tao_image");
    assert_eq!(res_biz_b2.csv_label, "Lỗi tạo image");

    let res_biz_b3 = resolve_step_failure(Step::B3, &business_fail, true, false);
    assert_eq!(res_biz_b3.db_status, "loi_tao_video");
    assert_eq!(res_biz_b3.csv_label, "Lỗi tạo video");
}

#[test]
fn test_p0_6_record_attempt_transient_vs_business() {
    let dir = make_temp_project("p06_record_attempt");
    let conn = crate::state_db::open(&dir).unwrap();
    conn.execute(
        "INSERT INTO runs(kind, status, started_at, total_rows) VALUES ('chung', 'dang_chay', '2026-01-01', 1)",
        [],
    ).unwrap();
    let run_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status, attempts_count)
         VALUES (?1, 'row1.png', 1, 'dang_cho', 'chua_chay', 'chua_chay', 0)",
        rusqlite::params![run_id],
    ).unwrap();
    let job_id = conn.last_insert_rowid();

    // 1. Transient attempt: count_toward_limit = false
    let total_after_transient = record_attempt(
        &conn,
        job_id,
        AttemptRecord {
            step: "B2",
            fallback_attempt_no: 1,
            result: "loi_tam_thoi",
            http_status: Some(502),
            error_code: "502",
            error_message: "Gateway Error",
            count_toward_limit: false,
        },
    );
    assert_eq!(total_after_transient, 0);
    assert_eq!(current_job_attempts(&conn, job_id), 0);

    // 2. Business attempt: count_toward_limit = true
    let total_after_biz = record_attempt(
        &conn,
        job_id,
        AttemptRecord {
            step: "B2",
            fallback_attempt_no: 2,
            result: "loi_nghiep_vu",
            http_status: None,
            error_code: "POLICY_VIOLATION",
            error_message: "Prompt refused",
            count_toward_limit: true,
        },
    );
    assert_eq!(total_after_biz, 1);
    assert_eq!(current_job_attempts(&conn, job_id), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_p0_6_reset_row_status_at_legacy_fallback() {
    let dir = make_temp_project("p06_reset_legacy");
    let conn = crate::state_db::open(&dir).unwrap();
    conn.execute(
        "INSERT INTO runs(kind, status, started_at, total_rows) VALUES ('chung', 'dang_chay', '2026-01-01', 1)",
        [],
    ).unwrap();
    let run_id = conn.last_insert_rowid();
    // Insert job with legacy row_key (image name without hash)
    conn.execute(
        "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status, attempts_count, last_error)
         VALUES (?1, 'photo.png', 1, 'loi_tao_image', 'loi', 'chua_chay', 3, 'Old error')",
        rusqlite::params![run_id],
    ).unwrap();

    // CSV has the row with full key
    prompts_csv::write_all(
        &dir.join("prompts.csv"),
        &[Row {
            row_no: 1,
            anh_tham_chieu: "photo.png".into(),
            promt_tao_image: "prompt img".into(),
            promt_tao_video: "prompt vid".into(),
            trang_thai: "Lỗi tạo image".into(),
            ghi_chu_loi: "Old error".into(),
            ..Default::default()
        }],
    )
    .unwrap();

    let full_key = crate::prompts_csv::row_key_of("photo.png", "prompt img", "prompt vid");
    let detail = crate::commands::reset_row_status_at(&dir, &full_key).expect("reset should succeed");

    assert_eq!(detail.row.status, "Đang chờ");
    assert_eq!(detail.row.attempts_count, 0);

    // Verify SQLite was updated via legacy key fallback
    let job = latest_job_info(&conn, "photo.png").unwrap();
    assert_eq!(job.status, "dang_cho");
    assert_eq!(job.attempts_count, 0);
    assert!(job.last_error.is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_p0_6_loi_tam_thoi_with_valid_image_plans_b3_and_inits_as_xong() {
    let dir = make_temp_project("loi_tam_thoi_init_img");
    let img_out = dir.join("image_out");
    std::fs::write(img_out.join("valid_gen.png"), b"image_data").unwrap();
    let cfg = Settings::default();
    let eng_cfg = EngineConfig {
        kind: "chung",
        row_keys: vec![],
        force_step: None,
        reset_error_attempts: false,
    };

    let row = Row {
        row_no: 1,
        anh_tham_chieu: "ref.png".into(),
        promt_tao_image: "p_img".into(),
        promt_tao_video: "p_vid".into(),
        trang_thai: "Lỗi tạm thời".into(),
        anh_da_tao: "valid_gen.png".into(),
        video_da_tao: "".into(),
        ..Default::default()
    };

    let plan = plan_row(&row, None, &cfg, &eng_cfg, &img_out, Some("valid_gen.png"), 1)
        .expect("must plan B3 for LoiTamThoi with valid image");
    assert_eq!(plan.steps, vec![Step::B3]);
    assert!(!plan.force_new_image);

    let (init_img_status, init_db_status) =
        initial_job_image_state(&row, row.status(), &plan, &img_out);
    assert_eq!(init_img_status, "xong");
    assert_eq!(init_db_status, "da_tao_image");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_find_previous_scene_video_found_and_missing() {
    let dir = make_temp_project("p2_1_find_prev");
    let csv_path = dir.join("prompts.csv");
    let video_out = dir.join("video_out");
    std::fs::create_dir_all(&video_out).unwrap();

    let conn = crate::state_db::open(&dir).unwrap();

    // 3 rows: row 1 (done), row 2 (noi_canh = 1), row 3 (noi_canh = 1)
    let row1 = Row {
        row_no: 2,
        anh_tham_chieu: "scene1.png".into(),
        promt_tao_image: "img1".into(),
        promt_tao_video: "vid1".into(),
        anh_da_tao: "scene1_gen.png".into(),
        video_da_tao: "scene1_v1.mp4".into(),
        trang_thai: "Đã tạo video".into(),
        noi_canh: false,
        ..Default::default()
    };
    let row2 = Row {
        row_no: 3,
        anh_tham_chieu: "scene2.png".into(),
        promt_tao_image: "img2".into(),
        promt_tao_video: "vid2".into(),
        anh_da_tao: "scene2_gen.png".into(),
        video_da_tao: "".into(),
        trang_thai: "Đang chờ".into(),
        noi_canh: true,
        ..Default::default()
    };
    let row3 = Row {
        row_no: 4,
        anh_tham_chieu: "scene3.png".into(),
        promt_tao_image: "img3".into(),
        promt_tao_video: "vid3".into(),
        anh_da_tao: "scene3_gen.png".into(),
        video_da_tao: "".into(),
        trang_thai: "Đang chờ".into(),
        noi_canh: true,
        ..Default::default()
    };
    prompts_csv::write_all(&csv_path, &[row1.clone(), row2.clone(), row3.clone()]).unwrap();

    // Create the video file for row 1 in video_out/
    let row1_vid = video_out.join("scene1_v1.mp4");
    std::fs::write(&row1_vid, b"fake-mp4-data").unwrap();

    // 1. row 1 is the first row -> has no previous scene
    let prev_for_row1 = find_previous_scene_video_impl(&csv_path, &video_out, &conn, 1, 1, &row1);
    assert!(prev_for_row1.is_none(), "first row should have no previous scene video");

    // 2. row 2 has row 1 preceding it with finished video -> finds scene1_v1.mp4
    let prev_for_row2 = find_previous_scene_video_impl(&csv_path, &video_out, &conn, 1, 2, &row2);
    assert_eq!(prev_for_row2, Some(row1_vid.clone()));

    // 3. row 3 has row 2 preceding it, but row 2 has NO video -> returns None (fail-safe)
    let prev_for_row3 = find_previous_scene_video_impl(&csv_path, &video_out, &conn, 1, 3, &row3);
    assert!(prev_for_row3.is_none(), "preceding row without video should return None");

    // 4. If row 1's video file is deleted from disk -> row 2 returns None
    std::fs::remove_file(&row1_vid).unwrap();
    let prev_after_delete = find_previous_scene_video_impl(&csv_path, &video_out, &conn, 1, 2, &row2);
    assert!(prev_after_delete.is_none(), "missing video on disk should return None");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_extract_last_frame_ffmpeg_error_handling() {
    let dir = make_temp_project("p2_1_ffmpeg");
    let non_existent_video = dir.join("not_found.mp4");
    let out_frame = dir.join(".cache").join("last_frame.png");

    // Non-existent source video must return Err
    let res = extract_last_frame_ffmpeg(&non_existent_video, &out_frame);
    assert!(res.is_err());
    assert!(res.unwrap_err().contains("không tồn tại"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_auto_merge_collect_video_paths_order_and_filtering() {
    let dir = make_temp_project("p2_2_collect");
    let csv_path = dir.join("prompts.csv");
    let video_out = dir.join("video_out");
    std::fs::create_dir_all(&video_out).unwrap();

    let row1 = prompts_csv::Row {
        anh_tham_chieu: "scene1.png".into(),
        video_da_tao: "scene1_v1.mp4".into(),
        ..Default::default()
    };
    let row2 = prompts_csv::Row {
        anh_tham_chieu: "scene2.png".into(),
        video_da_tao: "".into(), // Chưa có video
        ..Default::default()
    };
    let row3 = prompts_csv::Row {
        anh_tham_chieu: "scene3.png".into(),
        video_da_tao: "scene3_v1.mp4".into(),
        ..Default::default()
    };
    let row4 = prompts_csv::Row {
        anh_tham_chieu: "scene4.png".into(),
        video_da_tao: "scene4_missing.mp4".into(), // Có tên file nhưng file không tồn tại trên đĩa
        ..Default::default()
    };
    prompts_csv::write_all(&csv_path, &[row1.clone(), row2.clone(), row3.clone(), row4.clone()]).unwrap();

    // Tạo file thực tế cho row1 và row3
    let vid1 = video_out.join("scene1_v1.mp4");
    let vid3 = video_out.join("scene3_v1.mp4");
    std::fs::write(&vid1, b"mp4-content-1").unwrap();
    std::fs::write(&vid3, b"mp4-content-3").unwrap();

    // Thu thập toàn bộ: phải đúng thứ tự CSV và chỉ chứa các file thực sự tồn tại
    let collected = collect_batch_video_paths(&csv_path, &video_out).expect("collect videos");
    assert_eq!(collected.len(), 2, "chỉ 2 video tồn tại thực tế được thu thập");
    assert_eq!(collected[0], vid1, "video dòng 1 phải đứng đầu tiên");
    assert_eq!(collected[1], vid3, "video dòng 3 phải đứng thứ hai");

    // Thu thập có lọc filter_keys
    let mut filter = std::collections::HashSet::new();
    filter.insert("scene1.png".to_string());
    let filtered = collect_batch_video_paths_filtered(&csv_path, &video_out, Some(&filter)).expect("filtered collect");
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0], vid1);

    // Lỗi khi csv không tồn tại
    let non_csv = dir.join("non_existent.csv");
    assert!(collect_batch_video_paths(&non_csv, &video_out).is_err());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_auto_merge_command_formulation_and_silent_audio() {
    let input = Path::new("test_in.mp4");
    let output = Path::new("test_out.mp4");

    // 1. Trường hợp thiếu audio: phải có anullsrc và cờ -shortest
    let args_no_audio = build_normalize_clip_args(input, output, false, 720, 1280, 30);
    let joined_no_audio = args_no_audio.join(" ");
    assert!(joined_no_audio.contains("anullsrc=channel_layout=stereo:sample_rate=44100"), "phải bù anullsrc khi thiếu audio");
    assert!(joined_no_audio.contains("-shortest"), "phải có -shortest khi bù anullsrc");
    assert!(joined_no_audio.contains("scale=720:1280:force_original_aspect_ratio=decrease,pad=720:1280"));
    assert!(joined_no_audio.contains("fps=30"));
    assert!(joined_no_audio.contains("-c:a aac"));

    // 2. Trường hợp đã có audio: KHÔNG được chứa anullsrc và KHÔNG chứa -shortest
    let args_has_audio = build_normalize_clip_args(input, output, true, 1080, 1920, 24);
    let joined_has_audio = args_has_audio.join(" ");
    assert!(!joined_has_audio.contains("anullsrc"), "không bù anullsrc khi đã có audio");
    assert!(!joined_has_audio.contains("-shortest"), "không có -shortest khi không dùng infinite filter source");
    assert!(joined_has_audio.contains("scale=1080:1920"));
    assert!(joined_has_audio.contains("fps=24"));

    // 3. Concat demuxer command formulation
    let list_file = Path::new("list.txt");
    let concat_args = build_concat_demuxer_args(list_file, output);
    assert_eq!(concat_args[0], "-y");
    assert_eq!(concat_args[1], "-hide_banner");
    assert_eq!(concat_args[2], "-f");
    assert_eq!(concat_args[3], "concat");
    assert_eq!(concat_args[4], "-safe");
    assert_eq!(concat_args[5], "0");
    assert_eq!(concat_args[6], "-i");
    assert_eq!(concat_args[7], "list.txt");
    assert_eq!(concat_args[8], "-c");
    assert_eq!(concat_args[9], "copy");

    // 4. Filter complex concat command formulation
    let inps = vec![PathBuf::from("clip0.mp4"), PathBuf::from("clip1.mp4")];
    let has_a = vec![true, false];
    let fc_args = build_filter_complex_concat_args(&inps, output, &has_a, 720, 1280, 30);
    let joined_fc = fc_args.join(" ");
    assert!(joined_fc.contains("-filter_complex"));
    assert!(joined_fc.contains("concat=n=2:v=1:a=1"));
    assert!(joined_fc.contains("[outv]"));
    assert!(joined_fc.contains("[outa]"));
}

#[test]
fn test_auto_merge_ntfs_filename_sanitization_and_output_naming() {
    // Sanitize forbidden NTFS characters
    let dirty = "Project: <Special> \"Name\" / Path \\ With | Illegal * Chars ?";
    let clean = sanitize_ntfs_filename(dirty);
    assert!(!clean.contains(':'));
    assert!(!clean.contains('<'));
    assert!(!clean.contains('>'));
    assert!(!clean.contains('"'));
    assert!(!clean.contains('/'));
    assert!(!clean.contains('\\'));
    assert!(!clean.contains('|'));
    assert!(!clean.contains('*'));
    assert!(!clean.contains('?'));
    assert_eq!(clean, "Project_ _Special_ _Name_ _ Path _ With _ Illegal _ Chars _");

    // Output naming with valid project name
    let name1 = generate_merged_output_filename("MyStory", "20261120_120000");
    assert_eq!(name1, "MyStory_merged_20261120_120000.mp4");

    // Output naming with empty or whitespace project name
    let name2 = generate_merged_output_filename("", "20261120_120000");
    assert_eq!(name2, "merged_20261120_120000.mp4");

    let name3 = generate_merged_output_filename("   ", "20261120_120000");
    assert_eq!(name3, "merged_20261120_120000.mp4");

    // Dimensions parser
    assert_eq!(parse_video_dimensions("720x1280"), (720, 1280));
    assert_eq!(parse_video_dimensions("1080x1920"), (1080, 1920));
    assert_eq!(parse_video_dimensions("1280:720"), (1280, 720));
    assert_eq!(parse_video_dimensions("invalid"), (720, 1280));
    assert_eq!(parse_video_dimensions(""), (720, 1280));
}

#[test]
fn test_auto_merge_batch_fail_safe_and_clip_preservation() {
    let dir = make_temp_project("p2_2_failsafe");
    let csv_path = dir.join("prompts.csv");
    let video_out = dir.join("video_out");
    std::fs::create_dir_all(&video_out).unwrap();

    let row1 = prompts_csv::Row {
        anh_tham_chieu: "scene1.png".into(),
        video_da_tao: "clip1.mp4".into(),
        ..Default::default()
    };
    let row2 = prompts_csv::Row {
        anh_tham_chieu: "scene2.png".into(),
        video_da_tao: "clip2.mp4".into(),
        ..Default::default()
    };
    prompts_csv::write_all(&csv_path, &[row1.clone(), row2.clone()]).unwrap();

    let vid1 = video_out.join("clip1.mp4");
    let vid2 = video_out.join("clip2.mp4");
    std::fs::write(&vid1, b"original-content-1").unwrap();
    std::fs::write(&vid2, b"original-content-2").unwrap();

    let cfg = Settings {
        auto_merge: true,
        video_size: "720x1280".into(),
        ..Default::default()
    };

    // Khi các file video không phải là stream MP4 hợp lệ, auto_merge_batch bắt buộc trả về Err,
    // nhưng toàn bộ các clip gốc trong video_out/ phải hoàn toàn nguyên vẹn!
    let merge_res = auto_merge_batch(&dir, Some(1), None, &cfg);
    assert!(
        merge_res.is_err(),
        "auto_merge_batch phải báo lỗi khi clip giả không phải MP4 hợp lệ"
    );
    // Xác nhận clips lẻ vẫn nguyên vẹn 100%
    assert!(vid1.is_file(), "clip1 phải luôn được bảo toàn");
    assert!(vid2.is_file(), "clip2 phải luôn được bảo toàn");
    assert_eq!(std::fs::read(&vid1).unwrap(), b"original-content-1");
    assert_eq!(std::fs::read(&vid2).unwrap(), b"original-content-2");

    // Kiểm tra trường hợp chỉ có 1 video thành phẩm -> auto_merge_batch trả về Ok(None) bỏ qua an toàn
    let dir_single = make_temp_project("p2_2_single");
    let csv_single = dir_single.join("prompts.csv");
    let vout_single = dir_single.join("video_out");
    std::fs::create_dir_all(&vout_single).unwrap();
    prompts_csv::write_all(&csv_single, &[row1]).unwrap();
    std::fs::write(vout_single.join("clip1.mp4"), b"single-clip").unwrap();

    let single_res = auto_merge_batch(&dir_single, Some(1), None, &cfg);
    assert_eq!(single_res, Ok(None), "ít hơn 2 video phải trả về Ok(None)");

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir_single);
}

#[test]
fn test_auto_merge_probe_has_audio_error_on_missing_file() {
    let missing = Path::new("non_existent_video_file.mp4");
    let res = probe_has_audio_ffmpeg(missing);
    assert!(res.is_err());
    assert!(res.unwrap_err().contains("không tồn tại"));
}

#[test]
fn test_p0_2_rerun_row_step_isolation_for_multi_rows_sharing_same_image() {
    let dir = make_temp_project("p0_2_row_isolation");
    let img_out = dir.join("image_out");
    let cfg = Settings::default();

    // 2 dòng trong cùng CSV có cùng ảnh tham chiếu `anh_tham_chieu` nhưng prompt khác nhau
    let row1 = Row {
        row_no: 1,
        anh_tham_chieu: "shared_product.png".into(),
        promt_tao_image: "Prompt image 1 for shared product".into(),
        promt_tao_video: "Prompt video 1 for shared product".into(),
        trang_thai: "Đã tạo video".into(),
        anh_da_tao: "shared_product_gen1.png".into(),
        video_da_tao: "shared_product_gen1.mp4".into(),
        ..Default::default()
    };

    let row2 = Row {
        row_no: 2,
        anh_tham_chieu: "shared_product.png".into(),
        promt_tao_image: "Prompt image 2 for shared product (variant B)".into(),
        promt_tao_video: "Prompt video 2 for shared product (variant B)".into(),
        trang_thai: "Đã tạo video".into(),
        anh_da_tao: "shared_product_gen2.png".into(),
        video_da_tao: "shared_product_gen2.mp4".into(),
        ..Default::default()
    };

    let key1 = prompts_csv::row_key_of(
        &row1.anh_tham_chieu,
        &row1.promt_tao_image,
        &row1.promt_tao_video,
    );
    let key2 = prompts_csv::row_key_of(
        &row2.anh_tham_chieu,
        &row2.promt_tao_image,
        &row2.promt_tao_video,
    );

    assert_ne!(key1, key2, "Các dòng có prompt khác nhau phải có row_key phân biệt");
    assert!(key1.starts_with("shared_product.png#"));
    assert!(key2.starts_with("shared_product.png#"));

    // 1. Khi same_image = 2 (nhiều dòng dùng chung ảnh), truyền tên ảnh trần (`row.anh_tham_chieu`)
    // KHÔNG còn khớp nhầm sang các dòng anh em (triệt tiêu lỗi chạy chùm).
    let cfg_bare_image = EngineConfig {
        kind: "lam_lai",
        row_keys: vec![row1.anh_tham_chieu.clone()],
        force_step: Some(Step::B3),
        reset_error_attempts: false,
    };
    assert!(
        plan_row(&row1, None, &cfg, &cfg_bare_image, &img_out, None, 2).is_none(),
        "same_image=2 must block bare image matching on row 1"
    );
    assert!(
        plan_row(&row2, None, &cfg, &cfg_bare_image, &img_out, None, 2).is_none(),
        "same_image=2 must block bare image matching on row 2"
    );

    // 2. Kiểm chứng sau khi sửa: rerun_row_step truyền `key_of_row` (chứa hash)
    // Khi chạy lại dòng 1 (Step::B3):
    let cfg_isolated_b3 = EngineConfig {
        kind: "lam_lai",
        row_keys: vec![key1.clone()],
        force_step: Some(Step::B3),
        reset_error_attempts: false,
    };

    let plan1_b3 = plan_row(&row1, None, &cfg, &cfg_isolated_b3, &img_out, None, 2);
    assert!(plan1_b3.is_some(), "Dòng 1 được chọn chính xác");
    let p1 = plan1_b3.unwrap();
    assert_eq!(p1.steps, vec![Step::B3]);
    assert!(!p1.force_new_image);
    assert!(p1.force_new_video);

    let plan2_b3 = plan_row(&row2, None, &cfg, &cfg_isolated_b3, &img_out, None, 2);
    assert!(
        plan2_b3.is_none(),
        "Dòng 2 KHÔNG được chạy khi người dùng bấm chạy lại dòng 1 (đã triệt tiêu lỗi chạy chùm!)"
    );

    // Khi chạy lại dòng 1 (Step::B2):
    let cfg_isolated_b2 = EngineConfig {
        kind: "lam_lai",
        row_keys: vec![key1.clone()],
        force_step: Some(Step::B2),
        reset_error_attempts: false,
    };

    let plan1_b2 = plan_row(&row1, None, &cfg, &cfg_isolated_b2, &img_out, None, 2);
    assert!(plan1_b2.is_some(), "Dòng 1 được chọn chạy B2+B3");
    let p1_b2 = plan1_b2.unwrap();
    assert_eq!(p1_b2.steps, vec![Step::B2, Step::B3]);
    assert!(p1_b2.force_new_image);
    assert!(p1_b2.force_new_video);

    let plan2_b2 = plan_row(&row2, None, &cfg, &cfg_isolated_b2, &img_out, None, 2);
    assert!(
        plan2_b2.is_none(),
        "Dòng 2 KHÔNG được chạy khi người dùng bấm chạy lại B2 dòng 1"
    );

    // 3. Khi chạy lại dòng 2 với `key2`:
    let cfg_target_row2 = EngineConfig {
        kind: "lam_lai",
        row_keys: vec![key2.clone()],
        force_step: Some(Step::B3),
        reset_error_attempts: false,
    };

    assert!(
        plan_row(&row1, None, &cfg, &cfg_target_row2, &img_out, None, 2).is_none(),
        "Dòng 1 KHÔNG được chạy khi mục tiêu là dòng 2"
    );
    let plan2 = plan_row(&row2, None, &cfg, &cfg_target_row2, &img_out, None, 2);
    assert!(plan2.is_some(), "Dòng 2 được chọn chính xác");
    assert_eq!(plan2.unwrap().steps, vec![Step::B3]);

    // 4. Lập kế hoạch duyệt toàn bộ CSV rows chỉ chọn đúng 1 dòng mục tiêu
    let all_rows = [row1.clone(), row2.clone()];
    let planned_for_key1: Vec<_> = all_rows
        .iter()
        .filter_map(|r| plan_row(r, None, &cfg, &cfg_isolated_b3, &img_out, None, 2))
        .collect();
    assert_eq!(
        planned_for_key1.len(),
        1,
        "Chỉ duy nhất 1 dòng được lên lịch thực thi khi rerun_row_step dùng key_of_row"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_next_version_escapes_sqlite_like_wildcards() {
    let dir = make_temp_project("next_ver_like_escape");
    let conn = crate::state_db::open(&dir).unwrap();
    let img_out = dir.join("image_out");

    conn.execute(
        "INSERT INTO runs(kind, status, started_at, total_rows) VALUES ('chung', 'hoan_tat', '2026-11-18T00:00:00Z', 2)",
        [],
    )
    .unwrap();
    let run_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO jobs(run_id, row_key, status) VALUES (?1, 'other.png', 'da_tao_image')",
        rusqlite::params![run_id],
    )
    .unwrap();
    let j1 = conn.last_insert_rowid();

    // Các file có ký tự bình thường khớp wildcard '_' / '%' nếu không escape:
    // 'a_1' không escape sẽ khớp 'ab1_gen_v5.png'; '100%_sale' không escape sẽ khớp '100x_sale_gen_v9.png'.
    conn.execute(
        "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
         VALUES (?1, 'ab1.png', 'anh', 'ab1_gen_v5.png', 'image_out/ab1_gen_v5.png', 5, 1, '2026-11-18T00:01:00Z')",
        rusqlite::params![j1],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
         VALUES (?1, '100x_sale.png', 'anh', '100x_sale_gen_v9.png', 'image_out/100x_sale_gen_v9.png', 9, 1, '2026-11-18T00:02:00Z')",
        rusqlite::params![j1],
    )
    .unwrap();

    assert_eq!(next_version(&conn, &img_out, "a_1.png", "anh"), 1);
    assert_eq!(next_version(&conn, &img_out, "100%_sale.png", "anh"), 1);

    // Nếu thực sự có 'a_1_gen.png' (version 1) trong DB, next_version cho 'a_1.png' trả về 2
    conn.execute(
        "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
         VALUES (?1, 'a_1.png', 'anh', 'a_1_gen.png', 'image_out/a_1_gen.png', 1, 1, '2026-11-18T00:03:00Z')",
        rusqlite::params![j1],
    )
    .unwrap();
    assert_eq!(next_version(&conn, &img_out, "a_1.png", "anh"), 2);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_reserve_next_version_concurrent_same_stem_no_collision() {
    let dir = make_temp_project("reserve_concurrent");
    let img_out = dir.join("image_out");
    let _init_conn = crate::state_db::open(&dir).unwrap();

    let workers = 4usize;
    let barrier = Arc::new(std::sync::Barrier::new(workers));
    let reserved_versions = Arc::new(parking_lot::Mutex::new(Vec::<i64>::new()));

    std::thread::scope(|scope| {
        for idx in 0..workers {
            let dir_ref = &dir;
            let img_ref = &img_out;
            let barrier_ref = barrier.clone();
            let vers_ref = reserved_versions.clone();
            scope.spawn(move || {
                let conn = crate::state_db::open(dir_ref).unwrap();
                let rk = format!("shared_stem.png#h{idx}");
                barrier_ref.wait();
                let res = reserve_next_version(&conn, img_ref, &rk, "anh");
                vers_ref.lock().push(res.version);
                // Giữ reservation qua giai đoạn I/O mô phỏng trước khi ghi file & commit DB
                std::thread::sleep(Duration::from_millis(25));
                std::fs::write(&res.dest, format!("img-{idx}").as_bytes()).unwrap();
                conn.execute(
                    "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
                     VALUES (NULL, ?1, 'anh', ?2, ?3, ?4, 1, '2026-11-18T00:00:00Z')",
                    rusqlite::params![rk, res.file_name, format!("image_out/{}", res.file_name), res.version],
                )
                .unwrap();
                drop(res);
            });
        }
    });

    let mut got = reserved_versions.lock().clone();
    got.sort_unstable();
    assert_eq!(got, vec![1, 2, 3, 4], "concurrent workers sharing same stem must get distinct versions");

    // Kiểm tra Drop của VersionReservation nhả reservation khi không ghi đĩa/DB (nhánh lỗi)
    let conn = crate::state_db::open(&dir).unwrap();
    let aborted = reserve_next_version(&conn, &img_out, "shared_stem.png#abort", "anh");
    assert_eq!(aborted.version, 5);
    drop(aborted);
    let retried = reserve_next_version(&conn, &img_out, "shared_stem.png#retry", "anh");
    assert_eq!(retried.version, 5, "dropped reservation must release version 5 for reuse");
    drop(retried);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_e2e_transient_retries_not_starved_when_carried_attempts_at_max() {
    let dir = make_temp_project("e2e_transient_not_starved");
    std::fs::write(dir.join("input").join("cup.png"), sample_png_bytes(15, 25, 35)).unwrap();

    prompts_csv::write_all(
        &dir.join("prompts.csv"),
        &[Row {
            row_no: 2,
            anh_tham_chieu: "cup.png".into(),
            promt_tao_image: "cup img prompt".into(),
            promt_tao_video: "cup vid prompt".into(),
            trang_thai: "Đang chờ".into(),
            ..Default::default()
        }],
    )
    .unwrap();

    // Pre-seed job với carried_attempts = 3 == max_attempts = 3
    let cup_key = crate::prompts_csv::row_key_of("cup.png", "cup img prompt", "cup vid prompt");
    {
        let conn = crate::state_db::open(&dir).unwrap();
        conn.execute(
            "INSERT INTO runs(kind, status, started_at, total_rows) VALUES ('chung', 'hoan_tat', '2026-11-18T00:00:00Z', 1)",
            [],
        )
        .unwrap();
        let rid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status, image_prompt_hash, video_prompt_hash, attempts_count)
             VALUES (?1, ?2, 2, 'loi_tao_image', 'loi', 'chua_chay', ?3, ?4, 3)",
            rusqlite::params![rid, cup_key, sha2hex("cup img prompt"), sha2hex("cup vid prompt")],
        )
        .unwrap();
    }

    let (port_b, stop_b, join_b) = spawn_mock_http_server(move |_req| {
        RawHttpResp::binary(200, "OK", "video/mp4", sample_mp4_bytes(b"CUPV"))
    });

    let img_calls = Arc::new(AtomicUsize::new(0));
    let vid_calls = Arc::new(AtomicUsize::new(0));
    let img_calls_ref = img_calls.clone();
    let vid_calls_ref = vid_calls.clone();

    let (port_a, stop_a, join_a) = spawn_mock_http_server(move |req| {
        if req.method == "POST" && req.path == "/v1/images/edits" {
            let n = img_calls_ref.fetch_add(1, Ordering::SeqCst) + 1;
            if n <= 2 {
                return RawHttpResp::json(502, "Bad Gateway", serde_json::json!({ "error": "transient b2" }));
            }
            return RawHttpResp::json(
                200,
                "OK",
                serde_json::json!({ "data": [{ "b64_json": sample_jpeg_b64(10, 120, 220) }] }),
            );
        }
        if req.method == "POST" && req.path == "/v1/videos" {
            let n = vid_calls_ref.fetch_add(1, Ordering::SeqCst) + 1;
            if n <= 2 {
                return RawHttpResp::json(502, "Bad Gateway", serde_json::json!({ "error": "transient b3" }));
            }
            return RawHttpResp::json(201, "Created", serde_json::json!({ "id": "task_cup_1" }));
        }
        if req.method == "GET" && req.path == "/v1/videos/task_cup_1" {
            return RawHttpResp::json(
                200,
                "OK",
                serde_json::json!({
                    "status": "succeeded",
                    "video_url": format!("http://127.0.0.1:{port_b}/cup.mp4")
                }),
            );
        }
        RawHttpResp::json(404, "Not Found", serde_json::json!({}))
    });

    let api_base = format!("http://127.0.0.1:{port_a}/v1");
    adapters::register_test_api_key(&api_base, "sk-transient-test");
    let cfg = Settings {
        endpoint_image_base_url: api_base.clone(),
        endpoint_video_base_url: api_base,
        max_attempts: 3,
        backoff_s: [1, 1, 1],
        poll_interval_s: 1,
        poll_max_s: 20,
        ..Settings::default()
    };
    let noop_emit = |_name: &str, _val: serde_json::Value| {};
    let stop_flag = Arc::new(AtomicBool::new(false));
    let eng_cfg = EngineConfig {
        kind: "chung",
        row_keys: Vec::new(),
        force_step: None,
        reset_error_attempts: false,
    };

    let out = run_with_emitter(&noop_emit, &cfg, &dir, eng_cfg, &stop_flag)
        .expect("transient retries must not be starved when carried_attempts == max_attempts");
    assert_eq!(out.done, 1);
    assert_eq!(out.errors, 0);
    assert_eq!(img_calls.load(Ordering::SeqCst), 3, "B2 must retry twice and succeed on 3rd try");
    assert_eq!(vid_calls.load(Ordering::SeqCst), 3, "B3 must retry twice and succeed on 3rd try");
    assert!(dir.join("image_out").join("cup_gen.png").is_file());
    assert!(dir.join("video_out").join("cup_gen.mp4").is_file());

    stop_a.store(true, Ordering::SeqCst);
    stop_b.store(true, Ordering::SeqCst);
    let _ = join_a.join();
    let _ = join_b.join();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_plan_row_legacy_key_same_image_and_empty_img_prompt_b3_resume() {
    let dir = make_temp_project("plan_row_m7");
    let img_out = dir.join("image_out");
    let cfg = Settings::default();

    let empty_img_row = Row {
        row_no: 2,
        anh_tham_chieu: "shared.png".into(),
        promt_tao_image: "   ".into(),
        promt_tao_video: "animate shared".into(),
        anh_da_tao: "".into(),
        trang_thai: "Lỗi tạm thời".into(),
        ..Default::default()
    };

    // 1. same_image == 2 chặn bare image key, same_image == 1 cho phép, và khớp theo prev.row_key luôn hoạt động
    let bare_cfg = EngineConfig {
        kind: "lam_lai",
        row_keys: vec!["shared.png".into()],
        force_step: None,
        reset_error_attempts: false,
    };
    assert!(plan_row(&empty_img_row, None, &cfg, &bare_cfg, &img_out, None, 2).is_none());
    assert!(plan_row(&empty_img_row, None, &cfg, &bare_cfg, &img_out, None, 1).is_some());

    let prev_resume = LatestJobInfo {
        id: 10,
        row_key: "shared.png#oldhash".into(),
        status: "loi_tam_thoi".into(),
        image_status: "xong".into(),
        video_status: "loi".into(),
        image_prompt_hash: Some(sha2hex("")),
        video_prompt_hash: Some(sha2hex("animate shared")),
        attempts_count: 1,
        last_error: Some("502".into()),
        video_task_id: Some("task_resume_99".into()),
        video_download_url: Some("https://cdn.example.com/resumed.mp4".into()),
    };

    let prev_key_cfg = EngineConfig {
        kind: "lam_lai",
        row_keys: vec!["shared.png#oldhash".into()],
        force_step: None,
        reset_error_attempts: false,
    };
    let plan_resume = plan_row(
        &empty_img_row,
        Some(&prev_resume),
        &cfg,
        &prev_key_cfg,
        &img_out,
        None,
        2,
    )
    .expect("empty promt_tao_image + LoiTamThoi + prev.row_key must plan B3 resume");
    assert_eq!(plan_resume.steps, vec![Step::B3]);
    assert_eq!(plan_resume.resume_task_id.as_deref(), Some("task_resume_99"));
    assert_eq!(
        plan_resume.resume_download_url.as_deref(),
        Some("https://cdn.example.com/resumed.mp4")
    );

    // 2. Khi vid_changed với promt_tao_image rỗng và anh_da_tao rỗng -> vẫn chỉ chạy B3 (không chạy B2!)
    let mut vid_edited_row = empty_img_row.clone();
    vid_edited_row.promt_tao_video = "new video prompt".into();
    let plan_vid_changed = plan_row(
        &vid_edited_row,
        Some(&prev_resume),
        &cfg,
        &EngineConfig::default(),
        &img_out,
        None,
        1,
    )
    .expect("vid_changed with empty promt_tao_image must plan B3");
    assert_eq!(plan_vid_changed.steps, vec![Step::B3]);
    assert!(plan_vid_changed.force_new_video);
    assert!(plan_vid_changed.resume_task_id.is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_run_with_emitter_and_hook_defers_run_finished() {
    let dir = make_temp_project("defer_run_finished");
    std::fs::write(dir.join("input").join("solo.png"), sample_png_bytes(90, 80, 70)).unwrap();

    prompts_csv::write_all(
        &dir.join("prompts.csv"),
        &[Row {
            row_no: 2,
            anh_tham_chieu: "solo.png".into(),
            promt_tao_image: "".into(),
            promt_tao_video: "solo vid".into(),
            trang_thai: "Đã tạo image".into(),
            ..Default::default()
        }],
    )
    .unwrap();

    let (port_b, stop_b, join_b) = spawn_mock_http_server(move |_req| {
        RawHttpResp::binary(200, "OK", "video/mp4", sample_mp4_bytes(b"SOLO"))
    });
    let (port_a, stop_a, join_a) = spawn_mock_http_server(move |req| {
        if req.method == "POST" && req.path == "/v1/videos" {
            return RawHttpResp::json(201, "Created", serde_json::json!({ "id": "task_solo" }));
        }
        if req.method == "GET" && req.path == "/v1/videos/task_solo" {
            return RawHttpResp::json(
                200,
                "OK",
                serde_json::json!({
                    "status": "succeeded",
                    "video_url": format!("http://127.0.0.1:{port_b}/solo.mp4")
                }),
            );
        }
        RawHttpResp::json(404, "Not Found", serde_json::json!({}))
    });

    let api_base = format!("http://127.0.0.1:{port_a}/v1");
    adapters::register_test_api_key(&api_base, "sk-defer-test");
    let cfg = Settings {
        endpoint_image_base_url: api_base.clone(),
        endpoint_video_base_url: api_base,
        poll_interval_s: 1,
        poll_max_s: 20,
        ..Settings::default()
    };

    let events = Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
    let events_ref = events.clone();
    let emitter = move |name: &str, _val: serde_json::Value| {
        events_ref.lock().push(name.to_string());
    };
    let stop_flag = Arc::new(AtomicBool::new(false));

    let outcome = run_with_emitter_and_hook(
        &emitter,
        None,
        &cfg,
        &dir,
        EngineConfig::default(),
        &stop_flag,
    )
    .expect("run_with_emitter_and_hook must succeed");

    let hook_events = events.lock().clone();
    assert!(hook_events.iter().any(|e| e == "run_started"));
    assert!(
        !hook_events.iter().any(|e| e == "run_finished"),
        "run_with_emitter_and_hook MUST NOT emit run_finished before caller clears running flag"
    );
    assert_eq!(outcome.status, "hoan_tat");
    assert_eq!(outcome.done, 1);
    assert_eq!(outcome.errors, 0);
    assert!(!outcome.stopped);
    assert!(outcome.duration_ms >= 0);

    let payload = build_run_finished_payload(&outcome);
    assert_eq!(payload.get("run_id").and_then(|v| v.as_i64()), Some(outcome.run_id));
    assert_eq!(payload.get("status").and_then(|v| v.as_str()), Some("hoan_tat"));
    assert_eq!(payload.get("done_count").and_then(|v| v.as_u64()), Some(1));
    assert_eq!(payload.get("error_count").and_then(|v| v.as_u64()), Some(0));
    assert_eq!(
        payload.get("duration_ms").and_then(|v| v.as_i64()),
        Some(outcome.duration_ms)
    );

    stop_a.store(true, Ordering::SeqCst);
    stop_b.store(true, Ordering::SeqCst);
    let _ = join_a.join();
    let _ = join_b.join();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_plan_row_cho_empty_image_prompt_skips_to_b3() {
    let dir = make_temp_project("plan_row_cho_empty_img");
    let img_out = dir.join("image_out");
    let cfg = Settings::default();
    let row = Row {
        row_no: 1,
        anh_tham_chieu: "ref.png".into(),
        promt_tao_image: "   ".into(),
        promt_tao_video: "make it move".into(),
        anh_da_tao: "".into(),
        trang_thai: "Đang chờ".into(),
        ..Default::default()
    };
    let eng_cfg = EngineConfig {
        kind: "chung",
        row_keys: vec![],
        force_step: None,
        reset_error_attempts: false,
    };
    let plan = plan_row(&row, None, &cfg, &eng_cfg, &img_out, None, 1)
        .expect("Cho with empty image prompt must plan B3");
    assert_eq!(plan.steps, vec![Step::B3]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_run_b2_migrates_old_key_before_insert_and_preserves_single_current() {
    let dir = make_temp_project("run_b2_old_key_ver");
    let csv_path = dir.join("prompts.csv");
    let img_out = dir.join("image_out");
    let vid_out = dir.join("video_out");
    std::fs::write(dir.join("input").join("new_ref.png"), sample_png_bytes(10, 20, 30)).unwrap();
    std::fs::write(img_out.join("legacy_old_gen.png"), sample_png_bytes(1, 2, 3)).unwrap();

    let mut row = Row {
        row_no: 2,
        anh_tham_chieu: "new_ref.png".into(),
        promt_tao_image: "img prompt".into(),
        promt_tao_video: "vid prompt".into(),
        anh_da_tao: "legacy_old_gen.png".into(),
        trang_thai: "Đang chờ".into(),
        ..Default::default()
    };
    prompts_csv::write_all(&csv_path, std::slice::from_ref(&row)).unwrap();

    let old_row_key = "old_ref.png#abcd";
    let new_row_key = prompts_csv::row_key_of(&row.anh_tham_chieu, &row.promt_tao_image, &row.promt_tao_video);
    let conn = crate::state_db::open(&dir).unwrap();
    conn.execute(
        "INSERT INTO runs(kind, status, started_at, total_rows) VALUES ('chung', 'dang_chay', '2026-11-20T00:00:00Z', 1)",
        [],
    )
    .unwrap();
    let run_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status)
         VALUES (?1, ?2, 2, 'da_tao_image', 'xong', 'chua_chay')",
        rusqlite::params![run_id, old_row_key],
    )
    .unwrap();
    let old_job_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
         VALUES (?1, ?2, 'anh', 'legacy_old_gen.png', 'image_out/legacy_old_gen.png', 1, 1, '2026-11-20T00:00:01Z')",
        rusqlite::params![old_job_id, old_row_key],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status)
         VALUES (?1, ?2, 2, 'dang_cho', 'chua_chay', 'chua_chay')",
        rusqlite::params![run_id, new_row_key],
    )
    .unwrap();
    let new_job_id = conn.last_insert_rowid();

    let (port, stop_srv, join_srv) = spawn_mock_http_server(move |req| {
        if req.method == "POST" && req.path == "/v1/images/edits" {
            return RawHttpResp::json(
                200,
                "OK",
                serde_json::json!({ "data": [{ "b64_json": sample_jpeg_b64(50, 60, 70) }] }),
            );
        }
        RawHttpResp::json(404, "Not Found", serde_json::json!({}))
    });

    let api_base = format!("http://127.0.0.1:{port}/v1");
    adapters::register_test_api_key(&api_base, "sk-b2-old-key");
    let cfg = Settings {
        endpoint_image_base_url: api_base.clone(),
        endpoint_video_base_url: api_base,
        ..Settings::default()
    };
    let noop = |_name: &str, _val: serde_json::Value| {};
    let stop_flag = AtomicBool::new(false);
    let done_count = AtomicUsize::new(0);
    let active_rows = parking_lot::Mutex::new(Vec::new());
    let ctx = WorkerRowCtx {
        emitter: &noop,
        cfg: &cfg,
        conn: &conn,
        csv_path: &csv_path,
        image_out: &img_out,
        video_out: &vid_out,
        stop_flag: &stop_flag,
        run_id,
        job_id: new_job_id,
        total: 1,
        done_count: &done_count,
        active_rows: &active_rows,
    };

    let status = run_b2(&ctx, &mut row, true).expect("run_b2 should succeed");
    assert_eq!(status, RowStatus::DaTaoImage);
    assert_eq!(row.anh_da_tao, "new_ref_gen_v2.png");

    let refs: Vec<(String, String, i64, i64)> = {
        let mut stmt = conn
            .prepare("SELECT row_key, file_name, version, is_current FROM file_refs WHERE kind = 'anh' ORDER BY version ASC")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(|x| x.unwrap())
            .collect()
    };
    assert_eq!(
        refs,
        vec![
            (new_row_key.clone(), "legacy_old_gen.png".to_string(), 1, 0),
            (new_row_key.clone(), "new_ref_gen_v2.png".to_string(), 2, 1),
        ]
    );

    stop_srv.store(true, Ordering::SeqCst);
    let _ = join_srv.join();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_download_and_commit_video_migrates_old_key_before_insert_and_preserves_single_current() {
    let dir = make_temp_project("dl_vid_old_key_ver");
    let csv_path = dir.join("prompts.csv");
    let img_out = dir.join("image_out");
    let vid_out = dir.join("video_out");
    std::fs::write(img_out.join("old_clip_gen.png"), sample_png_bytes(1, 2, 3)).unwrap();
    std::fs::write(vid_out.join("old_clip_gen.mp4"), sample_mp4_bytes(b"OLD1")).unwrap();

    let mut row = Row {
        row_no: 2,
        anh_tham_chieu: "new_clip.png".into(),
        promt_tao_image: "".into(),
        promt_tao_video: "animate clip".into(),
        anh_da_tao: "old_clip_gen.png".into(),
        video_da_tao: "old_clip_gen.mp4".into(),
        trang_thai: "Đã tạo image".into(),
        ..Default::default()
    };
    prompts_csv::write_all(&csv_path, std::slice::from_ref(&row)).unwrap();

    let old_row_key = "old_clip.png#1234";
    let new_row_key = prompts_csv::row_key_of(&row.anh_tham_chieu, &row.promt_tao_image, &row.promt_tao_video);
    let conn = crate::state_db::open(&dir).unwrap();
    conn.execute(
        "INSERT INTO runs(kind, status, started_at, total_rows) VALUES ('chung', 'dang_chay', '2026-11-20T00:00:00Z', 1)",
        [],
    )
    .unwrap();
    let run_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status)
         VALUES (?1, ?2, 2, 'da_tao_video', 'xong', 'xong')",
        rusqlite::params![run_id, old_row_key],
    )
    .unwrap();
    let old_job_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
         VALUES (?1, ?2, 'anh', 'old_clip_gen.png', 'image_out/old_clip_gen.png', 1, 1, '2026-11-20T00:00:01Z')",
        rusqlite::params![old_job_id, old_row_key],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
         VALUES (?1, ?2, 'video', 'old_clip_gen.mp4', 'video_out/old_clip_gen.mp4', 1, 1, '2026-11-20T00:00:02Z')",
        rusqlite::params![old_job_id, old_row_key],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status)
         VALUES (?1, ?2, 2, 'da_tao_image', 'xong', 'dang_chay')",
        rusqlite::params![run_id, new_row_key],
    )
    .unwrap();
    let new_job_id = conn.last_insert_rowid();

    let (port, stop_srv, join_srv) = spawn_mock_http_server(move |_req| {
        RawHttpResp::binary(200, "OK", "video/mp4", sample_mp4_bytes(b"NEW2"))
    });

    let cfg = Settings {
        endpoint_video_base_url: format!("http://127.0.0.1:{port}/v1"),
        ..Settings::default()
    };
    let noop = |_name: &str, _val: serde_json::Value| {};
    let stop_flag = AtomicBool::new(false);
    let done_count = AtomicUsize::new(0);
    let active_rows = parking_lot::Mutex::new(Vec::new());
    let ctx = WorkerRowCtx {
        emitter: &noop,
        cfg: &cfg,
        conn: &conn,
        csv_path: &csv_path,
        image_out: &img_out,
        video_out: &vid_out,
        stop_flag: &stop_flag,
        run_id,
        job_id: new_job_id,
        total: 1,
        done_count: &done_count,
        active_rows: &active_rows,
    };

    let url = format!("http://127.0.0.1:{port}/out.mp4");
    download_and_commit_video(&ctx, &mut row, &url, "task_v2")
        .expect("download_and_commit_video should succeed");
    assert_eq!(row.video_da_tao, "new_clip_gen_v2.mp4");

    let video_refs: Vec<(String, String, i64, i64)> = {
        let mut stmt = conn
            .prepare("SELECT row_key, file_name, version, is_current FROM file_refs WHERE kind = 'video' ORDER BY version ASC")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(|x| x.unwrap())
            .collect()
    };
    assert_eq!(
        video_refs,
        vec![
            (new_row_key.clone(), "old_clip_gen.mp4".to_string(), 1, 0),
            (new_row_key.clone(), "new_clip_gen_v2.mp4".to_string(), 2, 1),
        ]
    );

    let img_ref: (String, i64, i64) = conn
        .query_row(
            "SELECT row_key, version, is_current FROM file_refs WHERE kind = 'anh'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(img_ref, (new_row_key, 1, 1));

    stop_srv.store(true, Ordering::SeqCst);
    let _ = join_srv.join();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_reserve_next_version_with_keys_accounts_for_colliding_old_key_bump() {
    let dir = make_temp_project("reserve_colliding_old_key");
    let img_out = dir.join("image_out");
    let conn = crate::state_db::open(&dir).unwrap();
    let target_key = "cur.png#h2";
    let old_key = "old.png#h1";

    conn.execute(
        "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
         VALUES (NULL, ?1, 'anh', 'cur_a.png', 'image_out/cur_a.png', 1, 1, '2026-11-20T00:00:00Z')",
        rusqlite::params![target_key],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
         VALUES (NULL, ?1, 'anh', 'old_b.png', 'image_out/old_b.png', 1, 1, '2026-11-20T00:00:01Z')",
        rusqlite::params![old_key],
    )
    .unwrap();

    let res =
        reserve_next_version_with_keys(&conn, &img_out, target_key, &[old_key.into()], "anh");
    assert_eq!(
        res.version, 3,
        "colliding v1 from old_key will bump to v2 during migration, so new ref must reserve v3"
    );
    drop(res);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_compute_pipeline_workers_matrix() {
    // Concurrency = 1, gateway -> 1 image + 1 video chạy song song độc lập
    let mut cfg = Settings {
        concurrency: 1,
        ..Settings::default()
    };
    assert_eq!(compute_pipeline_workers(&cfg, 5, 5, false, None), (1, 1));
    assert_eq!(compute_pipeline_workers(&cfg, 5, 5, true, None), (1, 1));

    // Concurrency = 2 -> 2 image + 2 video; khi có noi_canh -> 2 image + 1 video
    cfg.concurrency = 2;
    assert_eq!(compute_pipeline_workers(&cfg, 5, 5, false, None), (2, 2));
    assert_eq!(compute_pipeline_workers(&cfg, 5, 5, true, None), (2, 1));

    // Chỉ có B3 (b2_count = 0) -> không spawn worker ảnh
    assert_eq!(compute_pipeline_workers(&cfg, 0, 5, false, None), (0, 2));
    // Chỉ có B2 (b3_count = 0) -> không spawn worker video
    assert_eq!(compute_pipeline_workers(&cfg, 5, 0, false, None), (2, 0));

    // Nguồn video là Muse nội bộ: số luồng TUYỆT ĐỐI <= số tài khoản Muse khả dụng
    // để bảo đảm 1 tài khoản chỉ chạy tối đa 1 luồng video duy nhất và khớp hiển thị UI!
    cfg.concurrency = 5;
    cfg.image_source = "internal".into();
    cfg.video_source = "internal".into();
    // 1 tài khoản -> chặn cứng về (1 image, 1 video)
    assert_eq!(compute_pipeline_workers(&cfg, 10, 10, false, Some(1)), (1, 1));
    assert_eq!(compute_pipeline_workers(&cfg, 10, 10, false, None), (1, 1));
    assert_eq!(compute_pipeline_workers(&cfg, 10, 10, false, Some(0)), (1, 1));
    // 2 tài khoản -> (2 image, 2 video) (mỗi TK 1 video)
    assert_eq!(compute_pipeline_workers(&cfg, 10, 10, false, Some(2)), (2, 2));
    // 5 tài khoản -> (5 image, 5 video) (mỗi TK 1 video)
    assert_eq!(compute_pipeline_workers(&cfg, 10, 10, false, Some(5)), (5, 5));
    // 5 tài khoản + noi_canh -> video ép về 1 theo thứ tự CSV; image vẫn chạy song song 5
    assert_eq!(compute_pipeline_workers(&cfg, 10, 10, true, Some(5)), (5, 1));

    // Nguồn hỗn hợp (gateway ảnh + internal video): số luồng bị chặn <= số tài khoản Muse
    cfg.image_source = "gateway".into();
    cfg.video_source = "internal".into();
    assert_eq!(compute_pipeline_workers(&cfg, 10, 10, false, Some(1)), (1, 1));
    assert_eq!(compute_pipeline_workers(&cfg, 10, 10, false, Some(3)), (3, 3));
    assert_eq!(compute_pipeline_workers(&cfg, 10, 10, false, Some(6)), (5, 5));

    // Nguồn hỗn hợp (internal ảnh + gateway video): ảnh vẫn bị chặn trong ngưỡng 4 slot / TK Muse
    cfg.image_source = "internal".into();
    cfg.video_source = "gateway".into();
    cfg.concurrency = 10;
    assert_eq!(compute_pipeline_workers(&cfg, 10, 10, false, Some(1)), (4, 10));
    assert_eq!(compute_pipeline_workers(&cfg, 10, 0, false, Some(1)), (4, 0));
    assert_eq!(compute_pipeline_workers(&cfg, 10, 10, false, Some(3)), (10, 10));

    // Gateway ngoài cho cả ảnh và video -> chạy được > 3 luồng (tối đa MAX_CONCURRENCY)
    cfg.image_source = "gateway".into();
    cfg.video_source = "gateway".into();
    cfg.concurrency = 6;
    assert_eq!(compute_pipeline_workers(&cfg, 10, 10, false, Some(1)), (6, 6));
}

#[test]
fn test_e2e_decoupled_image_pipeline_runs_parallel_with_video_even_at_concurrency_1() {
    let dir = make_temp_project("e2e_decoupled_c1");
    for (name, rgb) in [
        ("r1.png", (10, 20, 30)),
        ("r2.png", (40, 50, 60)),
        ("r3.png", (70, 80, 90)),
    ] {
        std::fs::write(dir.join("input").join(name), sample_png_bytes(rgb.0, rgb.1, rgb.2))
            .unwrap();
    }

    prompts_csv::write_all(
        &dir.join("prompts.csv"),
        &[
            Row {
                row_no: 2,
                anh_tham_chieu: "r1.png".into(),
                promt_tao_image: "img 1".into(),
                promt_tao_video: "vid 1".into(),
                trang_thai: "Đang chờ".into(),
                ..Default::default()
            },
            Row {
                row_no: 3,
                anh_tham_chieu: "r2.png".into(),
                promt_tao_image: "img 2".into(),
                promt_tao_video: "vid 2".into(),
                trang_thai: "Đang chờ".into(),
                ..Default::default()
            },
            Row {
                row_no: 4,
                anh_tham_chieu: "r3.png".into(),
                promt_tao_image: "img 3".into(),
                promt_tao_video: "vid 3".into(),
                trang_thai: "Đang chờ".into(),
                ..Default::default()
            },
        ],
    )
    .unwrap();

    let (port_b, stop_b, join_b) = spawn_mock_http_server(move |_req| {
        RawHttpResp::binary(200, "OK", "video/mp4", sample_mp4_bytes(b"DECP"))
    });

    // Cổng đồng bộ chứng minh: video dòng 1 CHỈ hoàn tất sau khi CẢ 3 ảnh (r1, r2, r3) đã tạo xong!
    // Nếu B2 phải đợi B3 như kiến trúc cũ, r2 & r3 sẽ không bao giờ chạy trước khi video 1 xong.
    let images_completed = Arc::new(AtomicUsize::new(0));
    let all_images_seen_during_v1 = Arc::new(AtomicBool::new(false));
    let vid_seq = Arc::new(AtomicUsize::new(0));

    let img_cnt_ref = images_completed.clone();
    let seen_ref = all_images_seen_during_v1.clone();
    let vid_seq_ref = vid_seq.clone();

    let (port_a, stop_a, join_a) = spawn_mock_http_server(move |req| {
        if req.method == "POST" && req.path == "/v1/images/edits" {
            std::thread::sleep(Duration::from_millis(60));
            img_cnt_ref.fetch_add(1, Ordering::SeqCst);
            return RawHttpResp::json(
                200,
                "OK",
                serde_json::json!({ "data": [{ "b64_json": sample_jpeg_b64(30, 120, 210) }] }),
            );
        }
        if req.method == "POST" && req.path == "/v1/videos" {
            let n = vid_seq_ref.fetch_add(1, Ordering::SeqCst) + 1;
            return RawHttpResp::json(
                201,
                "Created",
                serde_json::json!({ "id": format!("task_dec_{n}") }),
            );
        }
        if req.method == "GET" && req.path.starts_with("/v1/videos/task_dec_") {
            if req.path == "/v1/videos/task_dec_1" {
                let deadline = Instant::now() + Duration::from_secs(6);
                while img_cnt_ref.load(Ordering::SeqCst) < 3 && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(25));
                }
                if img_cnt_ref.load(Ordering::SeqCst) >= 3 {
                    seen_ref.store(true, Ordering::SeqCst);
                }
            }
            return RawHttpResp::json(
                200,
                "OK",
                serde_json::json!({
                    "status": "succeeded",
                    "progress": 100,
                    "video_url": format!("http://127.0.0.1:{port_b}/dec.mp4")
                }),
            );
        }
        RawHttpResp::json(404, "Not Found", serde_json::json!({}))
    });

    let api_base = format!("http://127.0.0.1:{port_a}/v1");
    adapters::register_test_api_key(&api_base, "sk-decoupled-test");
    let cfg = Settings {
        endpoint_image_base_url: api_base.clone(),
        endpoint_video_base_url: api_base,
        concurrency: 1, // Ngay cả khi concurrency=1, ảnh & video vẫn chạy song song 2 tầng!
        poll_interval_s: 1,
        poll_max_s: 30,
        backoff_s: [1, 1, 1],
        ..Settings::default()
    };

    let saw_b2_and_b3_concurrent = Arc::new(AtomicBool::new(false));
    let conc_ref = saw_b2_and_b3_concurrent.clone();
    let emitter = move |name: &str, val: serde_json::Value| {
        if name == "progress" {
            if let Some(arr) = val.get("active_rows").and_then(|v| v.as_array()) {
                let has_b2 = arr
                    .iter()
                    .any(|r| r.get("step").and_then(|s| s.as_str()) == Some("B2"));
                let has_b3 = arr
                    .iter()
                    .any(|r| r.get("step").and_then(|s| s.as_str()) == Some("B3"));
                if has_b2 && has_b3 {
                    conc_ref.store(true, Ordering::SeqCst);
                }
            }
        }
    };
    let stop_flag = Arc::new(AtomicBool::new(false));

    let out = run_with_emitter(&emitter, &cfg, &dir, EngineConfig::default(), &stop_flag)
        .expect("decoupled concurrency=1 run must succeed");

    assert_eq!(out.done, 3);
    assert_eq!(out.errors, 0);
    assert!(
        all_images_seen_during_v1.load(Ordering::SeqCst),
        "r2 & r3 images MUST complete while r1 video is still in progress"
    );
    assert!(
        saw_b2_and_b3_concurrent.load(Ordering::SeqCst),
        "progress events must observe B2 and B3 running concurrently"
    );
    for stem in ["r1", "r2", "r3"] {
        assert!(dir.join("image_out").join(format!("{stem}_gen.png")).is_file());
        assert!(dir.join("video_out").join(format!("{stem}_gen.mp4")).is_file());
    }

    stop_a.store(true, Ordering::SeqCst);
    stop_b.store(true, Ordering::SeqCst);
    let _ = join_a.join();
    let _ = join_b.join();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_e2e_decoupled_pipeline_noi_canh_preserves_video_order_while_images_run_ahead() {
    let dir = make_temp_project("e2e_decoupled_noi_canh");
    for (name, rgb) in [
        ("s1.png", (10, 20, 30)),
        ("s2.png", (40, 50, 60)),
        ("s3.png", (70, 80, 90)),
    ] {
        std::fs::write(dir.join("input").join(name), sample_png_bytes(rgb.0, rgb.1, rgb.2))
            .unwrap();
    }

    prompts_csv::write_all(
        &dir.join("prompts.csv"),
        &[
            Row {
                row_no: 2,
                anh_tham_chieu: "s1.png".into(),
                promt_tao_image: "scene 1 img".into(),
                promt_tao_video: "scene 1 vid".into(),
                trang_thai: "Đang chờ".into(),
                noi_canh: false,
                ..Default::default()
            },
            Row {
                row_no: 3,
                anh_tham_chieu: "s2.png".into(),
                promt_tao_image: "scene 2 img".into(),
                promt_tao_video: "scene 2 vid".into(),
                trang_thai: "Đang chờ".into(),
                noi_canh: true,
                ..Default::default()
            },
            Row {
                row_no: 4,
                anh_tham_chieu: "s3.png".into(),
                promt_tao_image: "scene 3 img".into(),
                promt_tao_video: "scene 3 vid".into(),
                trang_thai: "Đang chờ".into(),
                noi_canh: true,
                ..Default::default()
            },
        ],
    )
    .unwrap();

    let downloads_completed = Arc::new(AtomicUsize::new(0));
    let dl_ref = downloads_completed.clone();
    let (port_b, stop_b, join_b) = spawn_mock_http_server(move |_req| {
        dl_ref.fetch_add(1, Ordering::SeqCst);
        RawHttpResp::binary(200, "OK", "video/mp4", sample_mp4_bytes(b"NOIC"))
    });

    let video_create_prompts: Arc<parking_lot::Mutex<Vec<String>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    let video_start_prev_downloads: Arc<parking_lot::Mutex<Vec<usize>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    let images_done = Arc::new(AtomicUsize::new(0));
    let images_done_before_s1_video_finish = Arc::new(AtomicUsize::new(0));

    let vprompts_ref = video_create_prompts.clone();
    let vprev_dl_ref = video_start_prev_downloads.clone();
    let dl_check_ref = downloads_completed.clone();
    let img_done_ref = images_done.clone();
    let img_before_s1_ref = images_done_before_s1_video_finish.clone();

    let (port_a, stop_a, join_a) = spawn_mock_http_server(move |req| {
        if req.method == "POST" && req.path == "/v1/images/edits" {
            let body_str = String::from_utf8_lossy(&req.body);
            // Cố tình làm chậm B2 của scene 1 hơn scene 2 để kiểm chứng hàng đợi video
            // vẫn giữ đúng thứ tự scene 1 -> scene 2 -> scene 3 (không để scene 2 nhảy trước!).
            if body_str.contains("scene 1 img") {
                std::thread::sleep(Duration::from_millis(250));
            } else {
                std::thread::sleep(Duration::from_millis(30));
            }
            img_done_ref.fetch_add(1, Ordering::SeqCst);
            return RawHttpResp::json(
                200,
                "OK",
                serde_json::json!({ "data": [{ "b64_json": sample_jpeg_b64(90, 150, 210) }] }),
            );
        }
        if req.method == "POST" && req.path == "/v1/videos" {
            let parsed: serde_json::Value =
                serde_json::from_slice(&req.body).unwrap_or_default();
            let prompt = parsed
                .get("prompt")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let idx = {
                let mut vp = vprompts_ref.lock();
                vp.push(prompt.clone());
                vp.len()
            };
            vprev_dl_ref.lock().push(dl_check_ref.load(Ordering::SeqCst));
            return RawHttpResp::json(
                201,
                "Created",
                serde_json::json!({ "id": format!("task_nc_{idx}") }),
            );
        }
        if req.method == "GET" && req.path.starts_with("/v1/videos/task_nc_") {
            if req.path == "/v1/videos/task_nc_1" {
                let deadline = Instant::now() + Duration::from_secs(5);
                while img_done_ref.load(Ordering::SeqCst) < 3 && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(20));
                }
                img_before_s1_ref.store(img_done_ref.load(Ordering::SeqCst), Ordering::SeqCst);
            }
            return RawHttpResp::json(
                200,
                "OK",
                serde_json::json!({
                    "status": "succeeded",
                    "progress": 100,
                    "video_url": format!("http://127.0.0.1:{port_b}/nc.mp4")
                }),
            );
        }
        RawHttpResp::json(404, "Not Found", serde_json::json!({}))
    });

    let api_base = format!("http://127.0.0.1:{port_a}/v1");
    adapters::register_test_api_key(&api_base, "sk-noicanh-test");
    let cfg = Settings {
        endpoint_image_base_url: api_base.clone(),
        endpoint_video_base_url: api_base,
        concurrency: 2,
        poll_interval_s: 1,
        poll_max_s: 30,
        backoff_s: [1, 1, 1],
        ..Settings::default()
    };
    let noop_emit = |_name: &str, _val: serde_json::Value| {};
    let stop_flag = Arc::new(AtomicBool::new(false));

    let out = run_with_emitter(&noop_emit, &cfg, &dir, EngineConfig::default(), &stop_flag)
        .expect("noi_canh decoupled run must succeed");
    assert_eq!(out.done, 3);
    assert_eq!(out.errors, 0);
    assert_eq!(
        images_done_before_s1_video_finish.load(Ordering::SeqCst),
        3,
        "all 3 images must finish ahead while scene 1 video is rendering"
    );
    assert_eq!(
        *video_create_prompts.lock(),
        vec![
            "scene 1 vid".to_string(),
            "scene 2 vid".to_string(),
            "scene 3 vid".to_string(),
        ],
        "noi_canh videos MUST run in strict CSV order even when scene 2 image finished before scene 1 image"
    );
    assert_eq!(
        *video_start_prev_downloads.lock(),
        vec![0, 1, 2],
        "each noi_canh video MUST wait for the previous scene's video download to complete"
    );

    stop_a.store(true, Ordering::SeqCst);
    stop_b.store(true, Ordering::SeqCst);
    let _ = join_a.join();
    let _ = join_b.join();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_e2e_decoupled_pipeline_b2_failure_does_not_block_or_deadlock_other_rows() {
    for ordered_noi_canh in [false, true] {
        let dir = make_temp_project(&format!("e2e_dec_b2_fail_{ordered_noi_canh}"));
        for (name, rgb) in [
            ("f1.png", (10, 20, 30)),
            ("f2.png", (40, 50, 60)),
            ("f3.png", (70, 80, 90)),
        ] {
            std::fs::write(
                dir.join("input").join(name),
                sample_png_bytes(rgb.0, rgb.1, rgb.2),
            )
            .unwrap();
        }

        prompts_csv::write_all(
            &dir.join("prompts.csv"),
            &[
                Row {
                    row_no: 2,
                    anh_tham_chieu: "f1.png".into(),
                    promt_tao_image: "bad prompt".into(),
                    promt_tao_video: "vid 1".into(),
                    trang_thai: "Đang chờ".into(),
                    noi_canh: false,
                    ..Default::default()
                },
                Row {
                    row_no: 3,
                    anh_tham_chieu: "f2.png".into(),
                    promt_tao_image: "good prompt 2".into(),
                    promt_tao_video: "vid 2".into(),
                    trang_thai: "Đang chờ".into(),
                    noi_canh: ordered_noi_canh,
                    ..Default::default()
                },
                Row {
                    row_no: 4,
                    anh_tham_chieu: "f3.png".into(),
                    promt_tao_image: "good prompt 3".into(),
                    promt_tao_video: "vid 3".into(),
                    trang_thai: "Đang chờ".into(),
                    noi_canh: ordered_noi_canh,
                    ..Default::default()
                },
            ],
        )
        .unwrap();

        let (port_b, stop_b, join_b) = spawn_mock_http_server(move |_req| {
            RawHttpResp::binary(200, "OK", "video/mp4", sample_mp4_bytes(b"FAIL"))
        });

        let vid_seq = Arc::new(AtomicUsize::new(0));
        let vid_seq_ref = vid_seq.clone();
        let (port_a, stop_a, join_a) = spawn_mock_http_server(move |req| {
            if req.method == "POST" && req.path == "/v1/images/edits" {
                let body_str = String::from_utf8_lossy(&req.body);
                if body_str.contains("bad prompt") {
                    return RawHttpResp::json(
                        400,
                        "Bad Request",
                        serde_json::json!({ "error": "rejected prompt" }),
                    );
                }
                return RawHttpResp::json(
                    200,
                    "OK",
                    serde_json::json!({ "data": [{ "b64_json": sample_jpeg_b64(10, 90, 180) }] }),
                );
            }
            if req.method == "POST" && req.path == "/v1/videos" {
                let n = vid_seq_ref.fetch_add(1, Ordering::SeqCst) + 1;
                return RawHttpResp::json(
                    201,
                    "Created",
                    serde_json::json!({ "id": format!("task_f_{n}") }),
                );
            }
            if req.method == "GET" && req.path.starts_with("/v1/videos/task_f_") {
                return RawHttpResp::json(
                    200,
                    "OK",
                    serde_json::json!({
                        "status": "succeeded",
                        "progress": 100,
                        "video_url": format!("http://127.0.0.1:{port_b}/f.mp4")
                    }),
                );
            }
            RawHttpResp::json(404, "Not Found", serde_json::json!({}))
        });

        let api_base = format!("http://127.0.0.1:{port_a}/v1");
        adapters::register_test_api_key(&api_base, "sk-b2fail-test");
        let cfg = Settings {
            endpoint_image_base_url: api_base.clone(),
            endpoint_video_base_url: api_base,
            concurrency: 2,
            poll_interval_s: 1,
            poll_max_s: 30,
            backoff_s: [1, 1, 1],
            ..Settings::default()
        };
        let noop_emit = |_name: &str, _val: serde_json::Value| {};
        let stop_flag = Arc::new(AtomicBool::new(false));

        let out = run_with_emitter(&noop_emit, &cfg, &dir, EngineConfig::default(), &stop_flag)
            .expect("run must complete cleanly when row 1 B2 fails");
        assert_eq!(out.status, "hoan_tat");
        assert_eq!(out.done, 2);
        assert_eq!(out.errors, 1);
        assert_eq!(vid_seq.load(Ordering::SeqCst), 2, "only 2 rows with valid images should call B3");
        assert!(!dir.join("video_out").join("f1_gen.mp4").exists());
        assert!(dir.join("video_out").join("f2_gen.mp4").is_file());
        assert!(dir.join("video_out").join("f3_gen.mp4").is_file());

        let conn = crate::state_db::open(&dir).unwrap();
        let f1_key = crate::prompts_csv::row_key_of("f1.png", "bad prompt", "vid 1");
        assert_eq!(
            latest_job_info(&conn, &f1_key).unwrap().status,
            "loi_tao_image"
        );

        stop_a.store(true, Ordering::SeqCst);
        stop_b.store(true, Ordering::SeqCst);
        let _ = join_a.join();
        let _ = join_b.join();
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn test_e2e_decoupled_pipeline_stop_mid_video_preserves_completed_ahead_images() {
    let dir = make_temp_project("e2e_dec_stop_resume");
    for (name, rgb) in [
        ("p1.png", (10, 20, 30)),
        ("p2.png", (40, 50, 60)),
        ("p3.png", (70, 80, 90)),
    ] {
        std::fs::write(dir.join("input").join(name), sample_png_bytes(rgb.0, rgb.1, rgb.2))
            .unwrap();
    }

    prompts_csv::write_all(
        &dir.join("prompts.csv"),
        &[
            Row {
                row_no: 2,
                anh_tham_chieu: "p1.png".into(),
                promt_tao_image: "p1 img".into(),
                promt_tao_video: "p1 vid".into(),
                trang_thai: "Đang chờ".into(),
                ..Default::default()
            },
            Row {
                row_no: 3,
                anh_tham_chieu: "p2.png".into(),
                promt_tao_image: "p2 img".into(),
                promt_tao_video: "p2 vid".into(),
                trang_thai: "Đang chờ".into(),
                ..Default::default()
            },
            Row {
                row_no: 4,
                anh_tham_chieu: "p3.png".into(),
                promt_tao_image: "p3 img".into(),
                promt_tao_video: "p3 vid".into(),
                trang_thai: "Đang chờ".into(),
                ..Default::default()
            },
        ],
    )
    .unwrap();

    let (port_b, stop_b, join_b) = spawn_mock_http_server(move |_req| {
        RawHttpResp::binary(200, "OK", "video/mp4", sample_mp4_bytes(b"STOP"))
    });

    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_on_first_poll = Arc::new(AtomicBool::new(true));
    let img_calls = Arc::new(AtomicUsize::new(0));
    let vid_create_calls = Arc::new(AtomicUsize::new(0));

    let stop_flag_srv = stop_flag.clone();
    let stop_poll_srv = stop_on_first_poll.clone();
    let img_calls_srv = img_calls.clone();
    let vid_calls_srv = vid_create_calls.clone();

    let (port_a, stop_a, join_a) = spawn_mock_http_server(move |req| {
        if req.method == "POST" && req.path == "/v1/images/edits" {
            img_calls_srv.fetch_add(1, Ordering::SeqCst);
            return RawHttpResp::json(
                200,
                "OK",
                serde_json::json!({ "data": [{ "b64_json": sample_jpeg_b64(25, 115, 205) }] }),
            );
        }
        if req.method == "POST" && req.path == "/v1/videos" {
            let n = vid_calls_srv.fetch_add(1, Ordering::SeqCst) + 1;
            return RawHttpResp::json(
                201,
                "Created",
                serde_json::json!({ "id": format!("task_sr_{n}") }),
            );
        }
        if req.method == "GET" && req.path.starts_with("/v1/videos/task_sr_") {
            if stop_poll_srv.load(Ordering::SeqCst) {
                // Đợi đủ cả 3 ảnh hoàn tất rồi mới kích hoạt STOP ngay giữa lượt poll video 1
                let deadline = Instant::now() + Duration::from_secs(5);
                while img_calls_srv.load(Ordering::SeqCst) < 3 && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(20));
                }
                stop_flag_srv.store(true, Ordering::SeqCst);
                return RawHttpResp::json(
                    200,
                    "OK",
                    serde_json::json!({ "status": "in_progress", "progress": 30 }),
                );
            }
            return RawHttpResp::json(
                200,
                "OK",
                serde_json::json!({
                    "status": "succeeded",
                    "progress": 100,
                    "video_url": format!("http://127.0.0.1:{port_b}/sr.mp4")
                }),
            );
        }
        RawHttpResp::json(404, "Not Found", serde_json::json!({}))
    });

    let api_base = format!("http://127.0.0.1:{port_a}/v1");
    adapters::register_test_api_key(&api_base, "sk-stop-resume-test");
    let cfg = Settings {
        endpoint_image_base_url: api_base.clone(),
        endpoint_video_base_url: api_base,
        concurrency: 1,
        poll_interval_s: 1,
        poll_max_s: 30,
        backoff_s: [1, 1, 1],
        ..Settings::default()
    };
    let noop_emit = |_name: &str, _val: serde_json::Value| {};

    // Lượt 1: dừng an toàn giữa lúc poll video dòng 1, sau khi cả 3 ảnh đã tạo xong phía trước
    let out1 = run_with_emitter(&noop_emit, &cfg, &dir, EngineConfig::default(), &stop_flag)
        .expect("stopped run must succeed cleanly");
    assert!(out1.stopped);
    assert_eq!(out1.status, "da_dung");
    assert_eq!(out1.done, 0);
    assert_eq!(out1.errors, 0);
    assert_eq!(img_calls.load(Ordering::SeqCst), 3);
    assert_eq!(vid_create_calls.load(Ordering::SeqCst), 1);

    {
        let conn = crate::state_db::open(&dir).unwrap();
        for (img, ip, vp) in [
            ("p1.png", "p1 img", "p1 vid"),
            ("p2.png", "p2 img", "p2 vid"),
            ("p3.png", "p3 img", "p3 vid"),
        ] {
            let k = crate::prompts_csv::row_key_of(img, ip, vp);
            let job = latest_job_info(&conn, &k).expect("job must be persisted");
            assert_eq!(job.status, "da_tao_image");
            assert_eq!(job.image_status, "xong");
        }
    }

    // Lượt 2: tiếp tục chạy — KHÔNG gọi tạo ảnh lại lần nào, resume task_sr_1 + tạo 2 video còn lại
    stop_on_first_poll.store(false, Ordering::SeqCst);
    stop_flag.store(false, Ordering::SeqCst);
    let out2 = run_with_emitter(&noop_emit, &cfg, &dir, EngineConfig::default(), &stop_flag)
        .expect("resumed run must succeed");
    assert_eq!(out2.status, "hoan_tat");
    assert_eq!(out2.done, 3);
    assert_eq!(out2.errors, 0);
    assert_eq!(
        img_calls.load(Ordering::SeqCst),
        3,
        "resumed run MUST NOT re-generate already completed images"
    );
    assert_eq!(
        vid_create_calls.load(Ordering::SeqCst),
        3,
        "task_sr_1 must be resumed without recreating, plus 2 new tasks for p2 and p3"
    );

    stop_a.store(true, Ordering::SeqCst);
    stop_b.store(true, Ordering::SeqCst);
    let _ = join_a.join();
    let _ = join_b.join();
    let _ = std::fs::remove_dir_all(&dir);
}
