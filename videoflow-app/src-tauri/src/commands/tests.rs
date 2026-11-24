use super::*;
use crate::prompts_csv::Row;
use std::sync::atomic::AtomicUsize;

static CMD_TEST_IDX: AtomicUsize = AtomicUsize::new(0);

fn temp_proj(tag: &str) -> PathBuf {
    let idx = CMD_TEST_IDX.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "videoflow_cmd_test_{}_{}_{}",
        tag,
        std::process::id(),
        idx
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    project::create_project(&dir, false).unwrap();
    dir
}

#[test]
fn test_skip_row_upserts_on_brand_new_row_without_prior_runs() {
    let dir = temp_proj("skip_new");
    prompts_csv::write_all(
        &dir.join("prompts.csv"),
        &[Row {
            row_no: 2,
            anh_tham_chieu: "fresh.png".into(),
            promt_tao_image: "img".into(),
            promt_tao_video: "vid".into(),
            ..Default::default()
        }],
    )
    .unwrap();

    let fresh_key = prompts_csv::row_key_of("fresh.png", "img", "vid");
    let detail = skip_row_at(&dir, &fresh_key).expect("skip brand-new row");
    assert_eq!(detail.row.status, "Bỏ qua");

    let dry = engine::dry_run(&dir, &Settings::default()).unwrap();
    assert_eq!(dry.total, 0);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_remap_conflict_and_delete_row_cleanup_and_run_detail_files() {
    let dir = temp_proj("remap_delete");
    std::fs::write(dir.join("input").join("old.png"), b"png1").unwrap();
    std::fs::write(dir.join("input").join("new.png"), b"png2").unwrap();
    std::fs::write(dir.join("image_out").join("old_gen.png"), b"gen_img").unwrap();
    std::fs::write(dir.join("video_out").join("old_gen.mp4"), b"gen_vid").unwrap();

    prompts_csv::write_all(
        &dir.join("prompts.csv"),
        &[Row {
            row_no: 2,
            anh_tham_chieu: "old.png".into(),
            promt_tao_image: "p1".into(),
            promt_tao_video: "p2".into(),
            anh_da_tao: "old_gen.png".into(),
            video_da_tao: "old_gen.mp4".into(),
            trang_thai: "Đã tạo video".into(),
            ..Default::default()
        }],
    )
    .unwrap();

    let run_id = {
        let conn = crate::state_db::open(&dir).unwrap();
        conn.execute(
            "INSERT INTO runs(kind, status, started_at, finished_at, total_rows, done_count, error_count)
             VALUES ('chung', 'hoan_tat', '2026-11-18T01:00:00Z', '2026-11-18T01:02:00Z', 1, 1, 0)",
            [],
        )
        .unwrap();
        let rid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status)
             VALUES (?1, 'old.png', 2, 'da_tao_video', 'xong', 'xong')",
            rusqlite::params![rid],
        )
        .unwrap();
        let jid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
             VALUES (?1, 'old.png', 'anh', 'old_gen.png', 'image_out/old_gen.png', 1, 1, '2026-11-18T01:00:30Z')",
            rusqlite::params![jid],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
             VALUES (?1, 'old.png', 'video', 'old_gen.mp4', 'video_out/old_gen.mp4', 1, 1, '2026-11-18T01:01:50Z')",
            rusqlite::params![jid],
        )
        .unwrap();

        // Bản ghi mồ côi của `new.png` ở `version = 2, is_current = 1` ([P2-5])
        conn.execute(
            "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status)
             VALUES (?1, 'new.png', 99, 'loi_tao_image', 'loi', 'chua_chay')",
            rusqlite::params![rid],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
             VALUES (NULL, 'new.png', 'anh', 'orphan_v2.png', 'image_out/orphan_v2.png', 2, 1, '2026-11-18T01:00:00Z')",
            [],
        )
        .unwrap();
        rid
    };

    let (summary, run_rows) = get_run_detail_at(&dir, run_id).unwrap();
    assert_eq!(summary.duration_ms, Some(120_000));
    // Bảng jobs trong test ghi khoá legacy ('old.png'); row detail hiển thị tên ảnh
    // suy ra từ khoá nên so qua image_name.
    let old_row = run_rows.iter().find(|r| r.image_name == "old.png").unwrap();
    assert_eq!(old_row.image_file, "old_gen.png");
    assert_eq!(old_row.video_file, "old_gen.mp4");

    // remap: dữ liệu legacy trong CSV nên khoá legacy vẫn tra được (ảnh duy nhất).
    let remapped = remap_reference_image_at(&dir, "old.png", "new.png").expect("remap ok");
    // Sau remap, ảnh cột A đổi nên khoá dòng đổi theo (tên ảnh mới + hash prompt cũ).
    assert_eq!(remapped.row.image_name, "new.png");
    assert!(remapped.row.row_key.starts_with("new.png#"));
    let new_key = remapped.row.row_key.clone();
    assert_eq!(
        remapped.row.image_file, "old_gen.png",
        "orphan file_ref v2 must be purged on remap"
    );

    assert!(resolve_safe_project_target(&dir, "image_out/old_gen.png").is_ok());
    assert!(resolve_safe_project_target(&dir, "../outside.txt").is_err());

    assert!(delete_row_at(&dir, &new_key).unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_save_row_prompts_trim_and_unlock_3_of_3() {
    let dir = temp_proj("save_prompts_hash");
    prompts_csv::write_all(
        &dir.join("prompts.csv"),
        &[
            Row {
                row_no: 2,
                anh_tham_chieu: "fresh.png".into(),
                promt_tao_image: "img0".into(),
                promt_tao_video: "vid0".into(),
                ..Default::default()
            },
            Row {
                row_no: 3,
                anh_tham_chieu: "locked.png".into(),
                promt_tao_image: "bad_prompt".into(),
                promt_tao_video: "vid_locked".into(),
                trang_thai: "Lỗi tạo image".into(),
                ..Default::default()
            },
        ],
    )
    .unwrap();

    let _ = skip_row_at(&dir, "fresh.png").unwrap();
    // Lưu kèm khoảng trắng ở cuối trên dòng chưa từng chạy -> không được bật cờ prompt_modified sai ([P2-2])
    let d_fresh =
        save_row_prompts_at(&dir, "fresh.png", "  img0_new  \n", " vid0_new ").unwrap();
    assert!(!d_fresh.row.prompt_modified);

    {
        let conn = crate::state_db::open(&dir).unwrap();
        conn.execute(
            "INSERT INTO runs(kind, status, started_at, total_rows) VALUES ('chung', 'hoan_tat', '2026-11-18T01:00:00Z', 1)",
            [],
        )
        .unwrap();
        let rid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status, image_prompt_hash, video_prompt_hash, attempts_count)
             VALUES (?1, 'locked.png', 3, 'loi_tao_image', 'loi', 'chua_chay', ?2, ?3, 3)",
            rusqlite::params![rid, engine::sha2hex("bad_prompt"), engine::sha2hex("vid_locked")],
        )
        .unwrap();
    }

    let d_locked =
        save_row_prompts_at(&dir, "locked.png", "fixed_prompt", "vid_locked").unwrap();
    assert!(d_locked.row.prompt_modified);
    assert_eq!(engine::dry_run(&dir, &Settings::default()).unwrap().total, 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_scan_import_set_version_pick_guard_and_history_export_filter() {
    let dir = temp_proj("full_option_cmds");
    let input_dir = dir.join("input");
    std::fs::write(input_dir.join("prod_a.png"), b"png_a").unwrap();
    std::fs::write(input_dir.join("prod_b.webp"), b"webp_b").unwrap();

    // 1) Quét ảnh chưa có trong prompts.csv & nhập hàng loạt
    let untracked = scan_untracked_input_images_at(&dir).unwrap();
    assert_eq!(untracked.len(), 2);

    let imported = import_input_images_at(
        &dir,
        &["prod_a.png".into(), "prod_b.webp".into()],
        "prompt anh mac dinh",
        "prompt video mac dinh",
    )
    .unwrap();
    assert_eq!(imported, 2);
    assert!(scan_untracked_input_images_at(&dir).unwrap().is_empty());

    // 2) [P0-4]: Chặn ghi đè file cùng tên trong input/ khi chọn từ thư mục ngoài
    let outside = dir.join("prod_a.png");
    std::fs::write(&outside, b"malicious_overwrite").unwrap();
    let err = copy_picked_image_into_input(&input_dir, &outside).unwrap_err();
    assert_eq!(err.code, "INPUT_FILE_EXISTS");
    assert_eq!(std::fs::read(input_dir.join("prod_a.png")).unwrap(), b"png_a");

    // 3) Chuyển đổi phiên bản sản phẩm chính (v1 <-> v2)
    std::fs::write(dir.join("image_out").join("prod_a_gen.png"), b"v1").unwrap();
    std::fs::write(dir.join("image_out").join("prod_a_gen_v2.png"), b"v2").unwrap();
    {
        let conn = crate::state_db::open(&dir).unwrap();
        conn.execute(
            "INSERT INTO runs(kind, status, started_at, total_rows) VALUES ('chung', 'hoan_tat', '2026-11-18T08:00:00+07:00', 1)",
            [],
        )
        .unwrap();
        let rid1 = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO runs(kind, status, started_at, total_rows) VALUES ('lam_lai', 'da_dung', '2026-11-18T09:00:00+07:00', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
             VALUES (NULL, 'prod_a.png', 'anh', 'prod_a_gen.png', 'image_out/prod_a_gen.png', 1, 0, '2026-11-18T08:00:00+07:00')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
             VALUES (NULL, 'prod_a.png', 'anh', 'prod_a_gen_v2.png', 'image_out/prod_a_gen_v2.png', 2, 1, '2026-11-18T09:00:00+07:00')",
            [],
        )
        .unwrap();
        let _ = rid1;
    }

    let switched = set_current_file_version_at(&dir, "prod_a.png", "image", 1).unwrap();
    assert_eq!(switched.row.image_file, "prod_a_gen.png");
    let csv_rows = prompts_csv::read_all(&dir.join("prompts.csv")).unwrap();
    let row_a = csv_rows
        .iter()
        .find(|r| r.anh_tham_chieu == "prod_a.png")
        .unwrap();
    assert_eq!(row_a.anh_da_tao, "prod_a_gen.png");

    // 4) [P2-4]: Xuất CSV lịch sử tôn trọng bộ lọc status & kind
    let out_csv = dir.join("history_filtered.csv");
    export_history_csv_at(
        &dir,
        Some(serde_json::json!({ "status": "da_dung", "kind": "lam_lai" })),
        &out_csv,
    )
    .unwrap();
    let exported_text = String::from_utf8(std::fs::read(&out_csv).unwrap()).unwrap();
    assert!(exported_text.contains("lam_lai"));
    assert!(!exported_text.contains(",chung,"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_ui3_probe_mp4_metadata_and_row_detail_fields() {
    let dir = temp_proj("ui3_video_detail");
    let non_exist = dir.join("missing.mp4");
    let (d_none, r_none) = probe_mp4_metadata(&non_exist);
    assert!(d_none.is_none());
    assert!(r_none.is_none());

    // Tạo mock MP4 file với mvhd (timescale=1000, duration=5000) và tkhd (720x1280)
    let mut mp4_bytes = Vec::new();
    // 1. ftyp box (20 bytes)
    mp4_bytes.extend_from_slice(&20u32.to_be_bytes());
    mp4_bytes.extend_from_slice(b"ftypisom");
    mp4_bytes.extend_from_slice(&1u32.to_be_bytes());
    mp4_bytes.extend_from_slice(b"isom");

    // 2. mvhd box (108 bytes)
    let mut mvhd_bytes = Vec::new();
    mvhd_bytes.extend_from_slice(&108u32.to_be_bytes());
    mvhd_bytes.extend_from_slice(b"mvhd");
    mvhd_bytes.push(0); // version 0
    mvhd_bytes.extend_from_slice(&[0, 0, 0]); // flags
    mvhd_bytes.extend_from_slice(&0u32.to_be_bytes()); // create_time
    mvhd_bytes.extend_from_slice(&0u32.to_be_bytes()); // mod_time
    mvhd_bytes.extend_from_slice(&1000u32.to_be_bytes()); // timescale = 1000
    mvhd_bytes.extend_from_slice(&5000u32.to_be_bytes()); // duration = 5000 (5.0s)
    mvhd_bytes.resize(108, 0); // pad rest of mvhd

    // 3. tkhd box (92 bytes)
    let mut tkhd_bytes = Vec::new();
    tkhd_bytes.extend_from_slice(&92u32.to_be_bytes());
    tkhd_bytes.extend_from_slice(b"tkhd");
    tkhd_bytes.push(0); // version 0
    tkhd_bytes.extend_from_slice(&[0, 0, 7]); // flags
    tkhd_bytes.resize(84, 0);
    tkhd_bytes.extend_from_slice(&(720u32 << 16).to_be_bytes()); // width = 720
    tkhd_bytes.extend_from_slice(&(1280u32 << 16).to_be_bytes()); // height = 1280
    assert_eq!(tkhd_bytes.len(), 92);

    // 4. trak box wrap tkhd
    let trak_size = (8 + tkhd_bytes.len()) as u32;
    let mut trak_bytes = Vec::new();
    trak_bytes.extend_from_slice(&trak_size.to_be_bytes());
    trak_bytes.extend_from_slice(b"trak");
    trak_bytes.extend_from_slice(&tkhd_bytes);

    // 5. moov box wrap mvhd + trak
    let moov_size = (8 + mvhd_bytes.len() + trak_bytes.len()) as u32;
    let mut moov_bytes = Vec::new();
    moov_bytes.extend_from_slice(&moov_size.to_be_bytes());
    moov_bytes.extend_from_slice(b"moov");
    moov_bytes.extend_from_slice(&mvhd_bytes);
    moov_bytes.extend_from_slice(&trak_bytes);

    mp4_bytes.extend_from_slice(&moov_bytes);

    let video_file = dir.join("video_out").join("test_sample.mp4");
    std::fs::write(&video_file, &mp4_bytes).unwrap();

    let (duration, resolution) = probe_mp4_metadata(&video_file);
    assert_eq!(duration, Some(5.0));
    assert_eq!(resolution, Some("720x1280".to_string()));

    // Thiết lập dự án và gọi build_row_detail_at
    let csv_path = dir.join("prompts.csv");
    std::fs::write(
        &csv_path,
        "anh_tham_chieu,promt_tao_image,promt_tao_video,anh_da_tao,video_da_tao,ngay_tao_idea,ngay_tao_anh,ngay_tao_video,trang_thai,ghi_chu_loi\n\
         test.png,prompt_img,prompt_vid,test_gen.png,test_sample.mp4,2026-11-20,2026-11-20,2026-11-20,Đã tạo video,\n",
    )
    .unwrap();

    let detail = build_row_detail_at(&dir, "test.png").unwrap();
    assert_eq!(detail.row.image_name, "test.png");
    assert_eq!(detail.row.video_file, "test_sample.mp4");
    assert_eq!(detail.prompt_image, "prompt_img");
    assert_eq!(detail.prompt_video, "prompt_vid");
    assert_eq!(detail.video_duration, Some(5.0));
    assert_eq!(detail.video_resolution, Some("720x1280".to_string()));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_issue_22_multi_row_same_image_save_prompt_remap_and_file_refs_lookup() {
    let dir = std::env::temp_dir().join(format!(
        "vf_issue22_{}_{}",
        std::process::id(),
        chrono::Local::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(dir.join("input")).unwrap();
    std::fs::create_dir_all(dir.join("image_out")).unwrap();
    std::fs::create_dir_all(dir.join("video_out")).unwrap();
    std::fs::write(dir.join("input").join("shared.png"), b"png1").unwrap();
    std::fs::write(dir.join("input").join("other.png"), b"png2").unwrap();
    std::fs::write(dir.join("image_out").join("shared_r1.png"), b"gen1").unwrap();

    let csv_path = dir.join("prompts.csv");
    std::fs::write(
        &csv_path,
        "anh_tham_chieu,promt_tao_image,promt_tao_video,anh_da_tao,video_da_tao,ngay_tao_idea,ngay_tao_anh,ngay_tao_video,trang_thai,ghi_chu_loi\n\
         shared.png,img_p1,vid_p1,shared_r1.png,,,,2026-11-20,Đã tạo image,\n\
         shared.png,img_p2,vid_p2,,,,,,Đang chờ,\n",
    )
    .unwrap();

    let k1 = prompts_csv::row_key_of("shared.png", "img_p1", "vid_p1");
    let k2 = prompts_csv::row_key_of("shared.png", "img_p2", "vid_p2");

    // Bỏ qua dòng 2 và ghi nhận job + file_ref cho dòng 1
    let _ = skip_row_at(&dir, &k2).unwrap();
    {
        let conn = crate::state_db::open(&dir).unwrap();
        let run_id = ensure_seed_run_id(&conn).unwrap();
        conn.execute(
            "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status, image_prompt_hash, video_prompt_hash, attempts_count)
             VALUES (?1, ?2, 2, 'da_tao_image', 'xong', 'chua_chay', ?3, ?4, 1)",
            rusqlite::params![run_id, k1, engine::sha2hex("img_p1"), engine::sha2hex("vid_p1")],
        )
        .unwrap();
        let jid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
             VALUES (?1, ?2, 'anh', 'shared_r1.png', 'image_out/shared_r1.png', 1, 1, '2026-11-20T10:00:00+07:00')",
            rusqlite::params![jid, k1],
        )
        .unwrap();
    }

    // Sửa prompt ảnh dòng 1 -> không được cướp/xóa trạng thái bo_qua của dòng 2
    let d1_after = save_row_prompts_at(&dir, &k1, "img_p1_edited", "vid_p1").unwrap();
    assert!(d1_after.row.prompt_modified, "dòng 1 phải bật prompt_modified sau khi sửa prompt ảnh");
    assert_eq!(d1_after.files.len(), 1, "không được mất file_refs lịch sử sau khi sửa prompt");

    let d2_check = build_row_detail_at(&dir, &k2).unwrap();
    assert_eq!(d2_check.row.status, "Bỏ qua", "dòng 2 dùng chung ảnh không được mất trạng thái Bỏ qua");

    // Remap dòng 2 sang other.png -> key mới có dạng other.png#hash và giữ nguyên dữ liệu dòng 1
    let d2_remapped = remap_reference_image_at(&dir, &k2, "other.png").unwrap();
    assert!(d2_remapped.row.row_key.starts_with("other.png#"));
    assert_eq!(d2_remapped.row.status, "Bỏ qua");

    let _ = std::fs::remove_dir_all(&dir);
}

fn read_http_req_test(stream: &mut std::net::TcpStream) -> (String, String) {
    use std::io::Read;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    let header_end;
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => {
                header_end = buf.len();
                break;
            }
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    header_end = pos + 4;
                    break;
                }
            }
            Err(_) => {
                header_end = buf.len();
                break;
            }
        }
    }
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let content_len = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            if k.trim().eq_ignore_ascii_case("content-length") {
                v.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    while buf.len() < header_end + content_len {
        match stream.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    }
    let body = String::from_utf8_lossy(
        &buf[header_end.min(buf.len())..(header_end + content_len).min(buf.len())],
    )
    .into_owned();
    (head, body)
}

fn write_http_json_test(stream: &mut std::net::TcpStream, status: &str, json_body: &str) {
    use std::io::Write;
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        json_body.len(),
        json_body
    );
    let _ = stream.write_all(resp.as_bytes());
}

#[test]
fn test_h2_enrich_account_json_with_proxy_clears_unassigned_and_skips_busy() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock sidecar");
    let port = listener.local_addr().unwrap().port();
    let captured = Arc::new(parking_lot::Mutex::new(Vec::<(String, String)>::new()));
    let captured_srv = captured.clone();

    let srv = std::thread::spawn(move || {
        for _ in 0..2 {
            if let Ok((mut stream, _)) = listener.accept() {
                let (head, body) = read_http_req_test(&mut stream);
                let req_line = head.lines().next().unwrap_or("").to_string();
                captured_srv.lock().push((req_line, body));
                write_http_json_test(&mut stream, "200 OK", r#"{"ok":true}"#);
            }
        }
    });

    let mut proxy_by_id = HashMap::new();
    proxy_by_id.insert(
        "px_1".to_string(),
        crate::proxies::ProxyItem {
            id: "px_1".into(),
            name: "P1".into(),
            kind: "http".into(),
            host: "198.51.100.1".into(),
            port: 8080,
            username: Some("u1".into()),
            password: None,
            has_password: true,
            note: None,
            last_status: "alive".into(),
            exit_ip: Some("198.51.100.1".into()),
            latency_ms: Some(12),
            tested_at: None,
            last_error: None,
            created_at: String::new(),
            updated_at: String::new(),
            assigned_account_ids: vec!["acc_1".into()],
        },
    );
    let mut acc_to_proxy = HashMap::new();
    acc_to_proxy.insert("acc_1".to_string(), "px_1".to_string());
    let mut proxy_urls = HashMap::new();
    proxy_urls.insert(
        "px_1".to_string(),
        "http://u1:real_pw@198.51.100.1:8080".to_string(),
    );

    let mut acc1 = serde_json::json!({ "id": "acc_1", "busy": false, "proxy": null });
    enrich_account_json_with_proxy(
        &mut acc1,
        &acc_to_proxy,
        &proxy_by_id,
        &proxy_urls,
        Some((port, "k1")),
    );
    assert_eq!(acc1["proxy"], "http://***@198.51.100.1:8080");
    assert_eq!(acc1["proxy_id"], "px_1");

    let mut acc2 = serde_json::json!({
        "id": "acc_2",
        "busy": false,
        "proxy": "http://***@1.2.3.4:8080"
    });
    enrich_account_json_with_proxy(
        &mut acc2,
        &acc_to_proxy,
        &proxy_by_id,
        &proxy_urls,
        Some((port, "k1")),
    );
    assert_eq!(acc2["proxy"], serde_json::Value::Null);
    assert_eq!(acc2["proxy_id"], serde_json::Value::Null);

    let mut acc3_busy = serde_json::json!({
        "id": "acc_3",
        "busy": true,
        "proxy": "http://***@1.2.3.4:8080"
    });
    enrich_account_json_with_proxy(
        &mut acc3_busy,
        &acc_to_proxy,
        &proxy_by_id,
        &proxy_urls,
        Some((port, "k1")),
    );
    assert_eq!(acc3_busy["proxy"], "http://***@1.2.3.4:8080");

    srv.join().unwrap();
    let reqs = captured.lock();
    assert_eq!(reqs.len(), 2);
    assert!(reqs[0].0.starts_with("PATCH /admin/accounts/acc_1"));
    assert!(reqs[0].1.contains("http://u1:real_pw@198.51.100.1:8080"));
    assert!(reqs[1].0.starts_with("PATCH /admin/accounts/acc_2"));
    assert_eq!(serde_json::from_str::<serde_json::Value>(&reqs[1].1).unwrap()["proxy"], "");
}

#[test]
fn test_h2_proxy_update_delete_fail_closed_and_rollback() {
    let db_path = std::env::temp_dir().join(format!(
        "vf_h2_rollback_{}_{}.db",
        std::process::id(),
        chrono::Local::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    let mut conn = crate::proxies::open_db_at(&db_path).expect("open db");
    let px = crate::proxies::create_proxy_in(
        &conn,
        &crate::proxies::ProxyInput {
            name: "OldProxy".into(),
            kind: "http".into(),
            host: "198.51.100.10".into(),
            port: 8080,
            username: Some("old_u".into()),
            password: Some("old_pw".into()),
            clear_password: None,
            note: None,
        },
    )
    .expect("create proxy");
    crate::proxies::set_account_proxy_in(&conn, "acc_a", Some(&px.id)).unwrap();
    crate::proxies::set_account_proxy_in(&conn, "acc_b", Some(&px.id)).unwrap();

    // 1. Closed port -> fail closed on admin_get, DB stays intact
    let closed_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_port = closed_listener.local_addr().unwrap().port();
    drop(closed_listener);
    let dead_sess = crate::muse_sidecar::SidecarSession {
        port: dead_port,
        api_key: "k".into(),
    };
    let err_upd = proxy_update_in_with_session(
        &mut conn,
        &px.id,
        &crate::proxies::ProxyInput {
            name: "NewProxy".into(),
            kind: "http".into(),
            host: "198.51.100.99".into(),
            port: 9090,
            username: None,
            password: None,
            clear_password: Some(true),
            note: None,
        },
        Some(&dead_sess),
    )
    .unwrap_err();
    assert_eq!(err_upd.code, "SIDECAR_SYNC_FAIL");
    assert_eq!(crate::proxies::get_proxy_in(&conn, &px.id).unwrap().name, "OldProxy");

    let err_del = proxy_delete_in_with_session(&mut conn, &px.id, Some(&dead_sess)).unwrap_err();
    assert_eq!(err_del.code, "SIDECAR_SYNC_FAIL");
    assert_eq!(crate::proxies::get_proxy_in(&conn, &px.id).unwrap().host, "198.51.100.10");

    // 2. Mock sidecar: update rolls back DB & acc_a when acc_b returns 409 AccountBusy;
    //    delete rolls back DB & acc_a on 409; then delete succeeds on 200 + 404 not_found.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let live_sess = crate::muse_sidecar::SidecarSession {
        port,
        api_key: "k".into(),
    };
    let log_reqs = Arc::new(parking_lot::Mutex::new(Vec::<(String, String)>::new()));
    let log_srv = log_reqs.clone();

    let srv = std::thread::spawn(move || {
        // Update (4 reqs): GET, PATCH acc_a (200), PATCH acc_b (409), rollback PATCH acc_a (200)
        // Delete-fail (4 reqs): GET, PATCH acc_a (200), PATCH acc_b (409), rollback PATCH acc_a (200)
        // Delete-ok (3 reqs): GET, PATCH acc_a (200), PATCH acc_b (404)
        for idx in 0..11 {
            if let Ok((mut stream, _)) = listener.accept() {
                let (head, body) = read_http_req_test(&mut stream);
                let req_line = head.lines().next().unwrap_or("").to_string();
                log_srv.lock().push((req_line.clone(), body));
                if req_line.starts_with("GET /admin/accounts") {
                    write_http_json_test(
                        &mut stream,
                        "200 OK",
                        r#"{"data":[{"id":"acc_a","busy":false},{"id":"acc_b","busy":false}]}"#,
                    );
                } else if idx == 2 || idx == 6 {
                    write_http_json_test(
                        &mut stream,
                        "409 Conflict",
                        r#"{"error":{"code":"account_busy","message":"Account busy"}}"#,
                    );
                } else if idx == 10 {
                    write_http_json_test(
                        &mut stream,
                        "404 Not Found",
                        r#"{"error":{"code":"not_found","message":"Account missing"}}"#,
                    );
                } else {
                    write_http_json_test(&mut stream, "200 OK", r#"{"ok":true}"#);
                }
            }
        }
    });

    let upd_conflict = proxy_update_in_with_session(
        &mut conn,
        &px.id,
        &crate::proxies::ProxyInput {
            name: "UpdatedShouldRollback".into(),
            kind: "socks5".into(),
            host: "198.51.100.77".into(),
            port: 1080,
            username: Some("new_u".into()),
            password: Some("new_pw".into()),
            clear_password: None,
            note: None,
        },
        Some(&live_sess),
    )
    .unwrap_err();
    assert_eq!(upd_conflict.code, "MUSE_ACCOUNT_BUSY");
    let after_rb = crate::proxies::get_proxy_in(&conn, &px.id).unwrap();
    assert_eq!(after_rb.name, "OldProxy");
    assert_eq!(after_rb.host, "198.51.100.10");
    assert_eq!(
        crate::proxies::resolve_proxy_url_in(&conn, &px.id).unwrap(),
        "http://old_u:old_pw@198.51.100.10:8080"
    );
    assert_eq!(after_rb.assigned_account_ids, vec!["acc_a".to_string(), "acc_b".to_string()]);

    let del_conflict =
        proxy_delete_in_with_session(&mut conn, &px.id, Some(&live_sess)).unwrap_err();
    assert_eq!(del_conflict.code, "MUSE_ACCOUNT_BUSY");
    assert_eq!(
        crate::proxies::get_proxy_in(&conn, &px.id)
            .unwrap()
            .assigned_account_ids,
        vec!["acc_a".to_string(), "acc_b".to_string()]
    );

    let deleted_accs =
        proxy_delete_in_with_session(&mut conn, &px.id, Some(&live_sess)).expect("delete ok");
    assert_eq!(deleted_accs, vec!["acc_a".to_string(), "acc_b".to_string()]);
    assert!(crate::proxies::get_proxy_in(&conn, &px.id).is_err());

    srv.join().unwrap();
    let captured = log_reqs.lock();
    assert_eq!(captured.len(), 11);
    assert!(captured[3].1.contains("http://old_u:old_pw@198.51.100.10:8080"));
    assert!(captured[7].1.contains("http://old_u:old_pw@198.51.100.10:8080"));

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_m2_proxy_test_all_concurrent_and_non_resurrecting() {
    let db_path = std::env::temp_dir().join(format!(
        "vf_m2_testall_{}_{}.db",
        std::process::id(),
        chrono::Local::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    let conn = crate::proxies::open_db_at(&db_path).expect("open db");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock proxy");
    let port = listener.local_addr().unwrap().port();
    // 2 proxies * 2 HTTP GETs each = 4 connections; barrier waits for at least 2 concurrent requests.
    let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let max_in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let in_f = in_flight.clone();
    let max_f = max_in_flight.clone();

    let (tx_done, rx_done) = std::sync::mpsc::channel::<()>();
    let srv = std::thread::spawn(move || {
        let mut handlers = Vec::new();
        for _ in 0..4 {
            if let Ok((mut stream, _)) = listener.accept() {
                let in_f_c = in_f.clone();
                let max_f_c = max_f.clone();
                handlers.push(std::thread::spawn(move || {
                    let (_head, _) = read_http_req_test(&mut stream);
                    let cur = in_f_c.fetch_add(1, Ordering::SeqCst) + 1;
                    max_f_c.fetch_max(cur, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(120));
                    in_f_c.fetch_sub(1, Ordering::SeqCst);
                    let body = "{\"ip\":\"198.51.100.50\"}";
                    write_http_json_test(&mut stream, "200 OK", body);
                }));
            }
        }
        for h in handlers {
            let _ = h.join();
        }
        let _ = tx_done.send(());
    });

    let p1 = crate::proxies::create_proxy_in(
        &conn,
        &crate::proxies::ProxyInput {
            name: "P1".into(),
            kind: "http".into(),
            host: "127.0.0.1".into(),
            port,
            username: None,
            password: None,
            clear_password: None,
            note: None,
        },
    )
    .unwrap();
    let p2 = crate::proxies::create_proxy_in(
        &conn,
        &crate::proxies::ProxyInput {
            name: "P2".into(),
            kind: "http".into(),
            host: "127.0.0.1".into(),
            port,
            username: None,
            password: None,
            clear_password: None,
            note: None,
        },
    )
    .unwrap();
    drop(conn);

    let tested = proxy_test_all_with_urls_in(
        &db_path,
        "http://example.com/",
        &["http://api.ipify.org/?format=json"],
    )
    .expect("proxy_test_all ok");
    rx_done.recv_timeout(Duration::from_secs(5)).unwrap();
    srv.join().unwrap();

    assert_eq!(tested.len(), 2);
    assert_eq!(tested[0].id, p1.id);
    assert_eq!(tested[1].id, p2.id);
    assert!(tested.iter().all(|p| p.last_status == "alive"));
    assert!(
        max_in_flight.load(Ordering::SeqCst) >= 2,
        "expected concurrent proxy tests, max_in_flight={}",
        max_in_flight.load(Ordering::SeqCst)
    );

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_m7_validate_export_save_path_and_log_redaction() {
    let dir = temp_proj("export_guard");
    let prompts_path = dir.join("prompts.csv");
    std::fs::write(&prompts_path, b"col1\n").unwrap();
    let state_db_path = dir.join("state.db");
    std::fs::write(&state_db_path, b"db").unwrap();

    let mut approved = HashSet::new();
    approved.insert(normalize_win_path_key(&prompts_path.to_string_lossy()));
    approved.insert(normalize_win_path_key(&state_db_path.to_string_lossy()));

    // Protected project files must be blocked even if present in approved_export_paths
    let err_prompts = validate_export_save_path_with_approved(
        &approved,
        &dir,
        &prompts_path.to_string_lossy(),
        &["csv"],
    )
    .unwrap_err();
    assert_eq!(err_prompts.code, "FORBIDDEN_EXPORT_PATH");

    let err_traversal = validate_export_save_path_with_approved(
        &approved,
        &dir,
        &format!("{}\\..\\evil.csv", dir.display()),
        &["csv"],
    )
    .unwrap_err();
    assert_eq!(err_traversal.code, "FORBIDDEN_EXPORT_PATH");

    let ok_inside = validate_export_save_path_with_approved(
        &HashSet::new(),
        &dir,
        &dir.join("history_out.csv").to_string_lossy(),
        &["csv"],
    );
    assert!(ok_inside.is_ok());

    crate::settings::register_secret_for_redaction("vf_secret_log_test_xyz");
    let items = vec![LogItem {
        id: 2,
        ts: "2026-11-20T10:00:00Z".into(),
        level: "error".into(),
        run_id: Some(9),
        row_key: Some("a.png".into()),
        step: Some("tao_video".into()),
        message: "fail with key vf_secret_log_test_xyz".into(),
        detail: Some("via http://alice:superPw@198.51.100.2:8080/api".into()),
    }];
    let txt = format_logs_txt_content(&items);
    assert!(!txt.contains("vf_secret_log_test_xyz"));
    assert!(!txt.contains("superPw"));
    assert!(txt.contains("http://alice:[REDACTED]@198.51.100.2:8080/api"));

    // Pass-2: NTFS ADS path (`prompts.csv:x.csv`) must be blocked even when in approved paths
    let ads_path = dir.join("prompts.csv:x.csv");
    let mut approved_ads = approved.clone();
    approved_ads.insert(normalize_win_path_key(&ads_path.to_string_lossy()));
    let err_ads = validate_export_save_path_with_approved(
        &approved_ads,
        &dir,
        &ads_path.to_string_lossy(),
        &["csv"],
    )
    .unwrap_err();
    assert_eq!(err_ads.code, "FORBIDDEN_EXPORT_PATH");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pass2_proxy_enrich_no_lie_and_metadata_only_update_while_busy() {
    let db_path = std::env::temp_dir().join(format!(
        "vf_cmd_pass2_{}_{}.db",
        std::process::id(),
        chrono::Local::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    let mut conn = crate::proxies::open_db_at(&db_path).unwrap();
    let px = crate::proxies::create_proxy_in(
        &conn,
        &crate::proxies::ProxyInput {
            name: "InitialName".into(),
            kind: "http".into(),
            host: "198.51.100.20".into(),
            port: 8080,
            username: Some("u".into()),
            password: Some("pw".into()),
            clear_password: None,
            note: Some("old note".into()),
        },
    )
    .unwrap();
    crate::proxies::set_account_proxy_in(&conn, "acc_1", Some(&px.id)).unwrap();
    let acc_to_proxy = crate::proxies::load_account_to_proxy_map(&conn).unwrap();
    let proxies = crate::proxies::list_proxies_in(&conn).unwrap();
    let proxy_by_id: HashMap<String, crate::proxies::ProxyItem> =
        proxies.into_iter().map(|p| (p.id.clone(), p)).collect();
    let (proxy_urls, _) =
        crate::proxies::resolve_proxy_urls_map_best_effort_in(&conn).unwrap();

    // 1. Busy account must NOT lie about obj["proxy"] when sidecar still holds old proxy
    let srv = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let srv_port = srv.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        use std::io::Read;
        // Only expects ONE request for the PATCH failure test
        if let Ok((mut s, _)) = srv.accept() {
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            write_http_json_test(
                &mut s,
                "500 Internal Server Error",
                r#"{"error":{"code":"sync_fail","message":"boom"}}"#,
            );
        }
    });
    let live_sess = crate::muse_sidecar::SidecarSession {
        port: srv_port,
        api_key: "k".into(),
    };

    let mut busy_acc = serde_json::json!({
        "id": "acc_1",
        "busy": true,
        "proxy": "http://***@old.invalid:3128"
    });
    enrich_account_json_with_proxy(
        &mut busy_acc,
        &acc_to_proxy,
        &proxy_by_id,
        &proxy_urls,
        Some((srv_port, "k")),
    );
    assert_eq!(
        busy_acc.get("proxy").and_then(|v| v.as_str()),
        Some("http://***@old.invalid:3128"),
        "busy account must retain actual sidecar proxy"
    );

    // Idle account where PATCH fails must also NOT lie about obj["proxy"]
    let mut fail_acc = serde_json::json!({
        "id": "acc_1",
        "busy": false,
        "proxy": "http://***@old.invalid:3128"
    });
    enrich_account_json_with_proxy(
        &mut fail_acc,
        &acc_to_proxy,
        &proxy_by_id,
        &proxy_urls,
        Some((srv_port, "k")),
    );
    server.join().unwrap();
    assert_eq!(
        fail_acc.get("proxy").and_then(|v| v.as_str()),
        Some("http://***@old.invalid:3128"),
        "failed PATCH must retain actual sidecar proxy"
    );

    // 2. Metadata-only proxy_update succeeds even while assigned account is busy (no sidecar traffic needed)
    let metadata_updated = proxy_update_in_with_session(
        &mut conn,
        &px.id,
        &crate::proxies::ProxyInput {
            name: "RenamedOnly".into(),
            kind: "http".into(),
            host: "198.51.100.20".into(),
            port: 8080,
            username: Some("u".into()),
            password: None,
            clear_password: None,
            note: Some("updated note".into()),
        },
        Some(&live_sess),
    )
    .expect("metadata-only update should not touch sidecar");
    assert_eq!(metadata_updated.name, "RenamedOnly");
    assert_eq!(metadata_updated.note.as_deref(), Some("updated note"));

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_muse_list_accounts_serializes_with_proxy_mutation_lock() {
    let db_path = std::env::temp_dir().join(format!(
        "vf_list_proxy_lock_{}_{}.db",
        std::process::id(),
        chrono::Local::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    let _ = std::fs::remove_file(&db_path);
    let conn = crate::proxies::open_db_at(&db_path).unwrap();
    let px_a = crate::proxies::create_proxy_in(
        &conn,
        &crate::proxies::ProxyInput {
            name: "ProxyA".into(),
            kind: "http".into(),
            host: "198.51.100.1".into(),
            port: 8001,
            username: Some("ua".into()),
            password: Some("pa".into()),
            clear_password: None,
            note: None,
        },
    )
    .unwrap();
    let px_b = crate::proxies::create_proxy_in(
        &conn,
        &crate::proxies::ProxyInput {
            name: "ProxyB".into(),
            kind: "http".into(),
            host: "198.51.100.2".into(),
            port: 8002,
            username: Some("ub".into()),
            password: Some("pb".into()),
            clear_password: None,
            note: None,
        },
    )
    .unwrap();
    crate::proxies::set_account_proxy_in(&conn, "acc_1", Some(&px_a.id)).unwrap();
    drop(conn);

    let sidecar_proxy = Arc::new(parking_lot::Mutex::new(px_a.to_masked_url()));
    let patch_history = Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
    let srv = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let srv_port = srv.local_addr().unwrap().port();
    srv.set_nonblocking(true).unwrap();
    let stop_srv = Arc::new(AtomicBool::new(false));
    let stop_srv_bg = Arc::clone(&stop_srv);
    let sidecar_proxy_bg = Arc::clone(&sidecar_proxy);
    let patch_history_bg = Arc::clone(&patch_history);
    let px_a_masked_srv = px_a.to_masked_url();
    let px_b_masked_srv = px_b.to_masked_url();

    let server = std::thread::spawn(move || {
        while !stop_srv_bg.load(Ordering::SeqCst) {
            match srv.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let (head, body) = read_http_req_test(&mut stream);
                    let first_line = head.lines().next().unwrap_or("");
                    if first_line.starts_with("GET /admin/accounts ") {
                        let cur_px = sidecar_proxy_bg.lock().clone();
                        let resp = serde_json::json!({
                            "data": [{ "id": "acc_1", "busy": false, "proxy": cur_px }]
                        })
                        .to_string();
                        write_http_json_test(&mut stream, "200 OK", &resp);
                    } else if first_line.starts_with("PATCH /admin/accounts/acc_1 ") {
                        let v: serde_json::Value =
                            serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
                        let raw_url = v
                            .get("proxy")
                            .and_then(|p| p.as_str())
                            .unwrap_or("")
                            .to_string();
                        let masked = if raw_url.contains("198.51.100.2:8002") {
                            px_b_masked_srv.clone()
                        } else if raw_url.contains("198.51.100.1:8001") {
                            px_a_masked_srv.clone()
                        } else {
                            raw_url.clone()
                        };
                        *sidecar_proxy_bg.lock() = masked.clone();
                        patch_history_bg.lock().push(raw_url);
                        let resp = serde_json::json!({
                            "account": { "id": "acc_1", "busy": false, "proxy": masked }
                        })
                        .to_string();
                        write_http_json_test(&mut stream, "200 OK", &resp);
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    let session = crate::muse_sidecar::SidecarSession {
        port: srv_port,
        api_key: "k".into(),
    };
    let (tx_midflight, rx_midflight) = std::sync::mpsc::channel::<()>();
    let db_calls = std::sync::atomic::AtomicUsize::new(0);
    let db_path_upd = db_path.clone();
    let px_b_id = px_b.id.clone();

    let list_handle = std::thread::spawn({
        let session_list = session.clone();
        let db_path_list = db_path.clone();
        move || {
            rx_midflight.recv_timeout(Duration::from_secs(3)).unwrap();
            muse_list_accounts_with_session_and_db(&session_list, || {
                crate::proxies::open_db_at(&db_path_list)
            })
        }
    });

    let upd_res = muse_update_account_with_session_and_db(
        "acc_1".into(),
        None,
        None,
        None,
        Some(px_b_id.clone()),
        || Ok((session.port, session.api_key.clone())),
        || {
            let n = db_calls.fetch_add(1, Ordering::SeqCst);
            if n == 1 {
                // Right after sidecar PATCH to ProxyB and BEFORE DB commit to ProxyB:
                // trigger concurrent muse_list_accounts; it must wait for proxy_mutation_lock.
                let _ = tx_midflight.send(());
                std::thread::sleep(Duration::from_millis(45));
            }
            crate::proxies::open_db_at(&db_path_upd)
        },
    )
    .expect("muse_update_account succeeds");
    assert_eq!(
        upd_res["account"]["proxy_id"].as_str(),
        Some(px_b_id.as_str())
    );

    let list_res = list_handle.join().unwrap().expect("muse_list_accounts succeeds");
    assert_eq!(
        list_res["data"][0]["proxy_id"].as_str(),
        Some(px_b_id.as_str())
    );
    assert_eq!(
        list_res["data"][0]["proxy"].as_str(),
        Some(px_b.to_masked_url().as_str())
    );

    let patches = patch_history.lock().clone();
    assert_eq!(
        patches.len(),
        1,
        "muse_list_accounts must NOT send a revert PATCH during in-flight update: {patches:?}"
    );
    assert_eq!(*sidecar_proxy.lock(), px_b.to_masked_url());

    stop_srv.store(true, Ordering::SeqCst);
    server.join().unwrap();
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_remap_reference_image_migrates_legacy_bare_key_and_logs() {
    let dir = temp_proj("remap_legacy_bare");
    std::fs::write(dir.join("input").join("old.png"), b"png1").unwrap();
    std::fs::write(dir.join("input").join("new.png"), b"png2").unwrap();
    std::fs::write(dir.join("image_out").join("old_gen.png"), b"gen_img").unwrap();
    std::fs::write(dir.join("video_out").join("old_gen.mp4"), b"gen_vid").unwrap();

    prompts_csv::write_all(
        &dir.join("prompts.csv"),
        &[Row {
            row_no: 2,
            anh_tham_chieu: "old.png".into(),
            promt_tao_image: "prompt img".into(),
            promt_tao_video: "prompt vid".into(),
            anh_da_tao: "old_gen.png".into(),
            video_da_tao: "old_gen.mp4".into(),
            trang_thai: "Đã tạo video".into(),
            ..Default::default()
        }],
    )
    .unwrap();

    let expected_old_key = prompts_csv::row_key_of("old.png", "prompt img", "prompt vid");
    {
        let conn = crate::state_db::open(&dir).unwrap();
        conn.execute(
            "INSERT INTO runs(kind, status, started_at, finished_at, total_rows, done_count, error_count)
             VALUES ('chung', 'hoan_tat', '2026-11-18T01:00:00Z', '2026-11-18T01:02:00Z', 1, 1, 0)",
            [],
        )
        .unwrap();
        let rid = conn.last_insert_rowid();
        // Seed v1.4.0 bare-image job, file_refs, and logs (even though CSV row has prompts -> expected_old_key has #hash)
        conn.execute(
            "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status, image_prompt_hash, video_prompt_hash)
             VALUES (?1, 'old.png', 2, 'da_tao_video', 'xong', 'xong', ?2, ?3)",
            rusqlite::params![rid, engine::sha2hex("prompt img"), engine::sha2hex("prompt vid")],
        )
        .unwrap();
        let jid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
             VALUES (?1, 'old.png', 'anh', 'old_gen.png', 'image_out/old_gen.png', 1, 1, '2026-11-18T01:00:30Z')",
            rusqlite::params![jid],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
             VALUES (?1, 'old.png', 'video', 'old_gen.mp4', 'video_out/old_gen.mp4', 1, 1, '2026-11-18T01:01:30Z')",
            rusqlite::params![jid],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO logs(ts, level, run_id, row_key, step, message)
             VALUES ('2026-11-18T01:01:00Z', 'info', ?1, 'old.png', 'B2', 'legacy log')",
            rusqlite::params![rid],
        )
        .unwrap();
    }

    // Caller passes the hashed row_key (as UI does) when remapping to new.png
    let remapped = remap_reference_image_at(&dir, &expected_old_key, "new.png").unwrap();
    let new_key = prompts_csv::row_key_of("new.png", "prompt img", "prompt vid");
    assert_eq!(remapped.row.row_key, new_key);
    assert_eq!(remapped.row.status, "Đã tạo video");
    assert_eq!(remapped.row.image_file, "old_gen.png");
    assert_eq!(remapped.row.video_file, "old_gen.mp4");
    assert_eq!(remapped.files.len(), 2);

    let conn = crate::state_db::open(&dir).unwrap();
    let old_jobs_cnt: i64 = conn
        .query_row("SELECT COUNT(*) FROM jobs WHERE LOWER(row_key) = 'old.png'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(old_jobs_cnt, 0);
    let new_job_status: String = conn
        .query_row(
            "SELECT status FROM jobs WHERE row_key = ?1",
            rusqlite::params![new_key],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(new_job_status, "da_tao_video");
    let new_refs_cnt: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM file_refs WHERE row_key = ?1",
            rusqlite::params![new_key],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(new_refs_cnt, 2);
    let new_logs_cnt: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM logs WHERE row_key = ?1",
            rusqlite::params![new_key],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(new_logs_cnt, 1);

    // Same-key remap branch (e.g. legacy 'new.png' bare row_key migrated on same-image remap)
    conn.execute(
        "INSERT INTO logs(ts, level, row_key, step, message) VALUES ('2026-11-18T01:02:00Z', 'info', 'new.png', 'B3', 'bare log')",
        [],
    )
    .unwrap();
    drop(conn);
    let same_remapped = remap_reference_image_at(&dir, &new_key, "new.png").unwrap();
    assert_eq!(same_remapped.row.row_key, new_key);
    let conn2 = crate::state_db::open(&dir).unwrap();
    let migrated_logs: i64 = conn2
        .query_row(
            "SELECT COUNT(*) FROM logs WHERE row_key = ?1",
            rusqlite::params![new_key],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(migrated_logs, 2);

    drop(conn2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(windows)]
#[test]
fn test_csv_locked_aborts_before_state_db_commit_in_version_reset_and_skip() {
    use std::os::windows::fs::OpenOptionsExt;

    let dir = temp_proj("csv_lock_no_db_split_brain");
    std::fs::write(dir.join("input").join("item.png"), b"png").unwrap();
    std::fs::write(dir.join("image_out").join("item_gen.png"), b"v1").unwrap();
    std::fs::write(dir.join("image_out").join("item_gen_v2.png"), b"v2").unwrap();

    let csv_path = dir.join("prompts.csv");
    prompts_csv::write_all(
        &csv_path,
        &[
            Row {
                row_no: 2,
                anh_tham_chieu: "item.png".into(),
                promt_tao_image: "p_img".into(),
                promt_tao_video: "p_vid".into(),
                anh_da_tao: "item_gen_v2.png".into(),
                trang_thai: "Lỗi tạo video".into(),
                ghi_chu_loi: "old error".into(),
                ..Default::default()
            },
            Row {
                row_no: 3,
                anh_tham_chieu: "fresh.png".into(),
                promt_tao_image: "f_img".into(),
                promt_tao_video: "f_vid".into(),
                ..Default::default()
            },
        ],
    )
    .unwrap();

    let item_key = prompts_csv::row_key_of("item.png", "p_img", "p_vid");
    let fresh_key = prompts_csv::row_key_of("fresh.png", "f_img", "f_vid");
    let item_job_id = {
        let conn = crate::state_db::open(&dir).unwrap();
        conn.execute(
            "INSERT INTO runs(kind, status, started_at, total_rows) VALUES ('chung', 'hoan_tat', '2026-11-20T00:00:00Z', 1)",
            [],
        )
        .unwrap();
        let rid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status, image_prompt_hash, video_prompt_hash, video_task_id, attempts_count, last_error)
             VALUES (?1, ?2, 2, 'loi_tao_video', 'xong', 'loi', 'h_img', 'h_vid', 'task_abc', 2, 'boom')",
            rusqlite::params![rid, item_key],
        )
        .unwrap();
        let jid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
             VALUES (?1, ?2, 'anh', 'item_gen.png', 'image_out/item_gen.png', 1, 0, '2026-11-20T00:00:01Z')",
            rusqlite::params![jid, item_key],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
             VALUES (?1, ?2, 'anh', 'item_gen_v2.png', 'image_out/item_gen_v2.png', 2, 1, '2026-11-20T00:00:02Z')",
            rusqlite::params![jid, item_key],
        )
        .unwrap();
        jid
    };

    // Hold FILE_SHARE_READ-only Windows handle (no write/delete share, matching Excel lock):
    // read_all succeeds, rename fails with CSV_LOCKED.
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&csv_path)
        .expect("acquire exclusive write/delete lock on prompts.csv");

    let e_ver = set_current_file_version_at(&dir, &item_key, "image", 1)
        .err()
        .expect("set_current_file_version_at must fail on locked csv");
    assert_eq!(e_ver.code, "CSV_LOCKED");

    let e_reset = reset_row_status_at(&dir, &item_key)
        .err()
        .expect("reset_row_status_at must fail on locked csv");
    assert_eq!(e_reset.code, "CSV_LOCKED");

    let e_skip_existing = skip_row_at(&dir, &item_key)
        .err()
        .expect("skip_row_at existing must fail on locked csv");
    assert_eq!(e_skip_existing.code, "CSV_LOCKED");

    let e_skip_fresh = skip_row_at(&dir, &fresh_key)
        .err()
        .expect("skip_row_at fresh must fail on locked csv");
    assert_eq!(e_skip_fresh.code, "CSV_LOCKED");

    drop(lock_file);

    // Verify state.db is completely untouched across file_refs, jobs, and runs
    let conn = crate::state_db::open(&dir).unwrap();
    let cur_ver: i64 = conn
        .query_row(
            "SELECT version FROM file_refs WHERE row_key = ?1 AND kind = 'anh' AND is_current = 1",
            rusqlite::params![item_key],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cur_ver, 2, "file_refs.is_current must stay at v2");

    let (st, img_st, vid_st, task_id, attempts, err_msg): (
        String,
        String,
        String,
        Option<String>,
        i64,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT status, image_status, video_status, video_task_id, attempts_count, last_error FROM jobs WHERE id = ?1",
            rusqlite::params![item_job_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .unwrap();
    assert_eq!(st, "loi_tao_video");
    assert_eq!(img_st, "xong");
    assert_eq!(vid_st, "loi");
    assert_eq!(task_id.as_deref(), Some("task_abc"));
    assert_eq!(attempts, 2);
    assert_eq!(err_msg.as_deref(), Some("boom"));

    let fresh_jobs: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM jobs WHERE row_key = ?1",
            rusqlite::params![fresh_key],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fresh_jobs, 0, "skip_row_at must not insert job on CSV_LOCKED");
    let runs_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(runs_count, 1, "skip_row_at must not insert seed run on CSV_LOCKED");

    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_shutdown_app_state_orders_stop_join_and_recover_with_bounded_wait() {
    let dir = temp_proj("shutdown_order");
    let run_id = {
        let conn = crate::state_db::open(&dir).unwrap();
        conn.execute(
            "INSERT INTO runs(kind, status, started_at, total_rows) VALUES ('chung', 'dang_chay', '2026-11-20T00:00:00Z', 1)",
            [],
        )
        .unwrap();
        let rid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status)
             VALUES (?1, 'k.png', 2, 'dang_cho', 'dang_chay', 'chua_chay')",
            rusqlite::params![rid],
        )
        .unwrap();
        rid
    };

    let events = Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
    let stop_flag = Arc::new(AtomicBool::new(false));

    let state = AppState {
        settings: SettingsStore::with_path(dir.join("settings.json")),
        settings_cache: parking_lot::RwLock::new(Settings::default()),
        project: parking_lot::RwLock::new(Some(dir.clone())),
        last_probed_dir: parking_lot::RwLock::new(None),
        approved_export_paths: parking_lot::RwLock::new(HashSet::new()),
        running: AtomicBool::new(true),
        exit_requested: AtomicBool::new(false),
        active_run_id: AtomicI64::new(run_id),
        stop_flag: parking_lot::RwLock::new(Some(Arc::clone(&stop_flag))),
        engine_thread: parking_lot::Mutex::new(None),
    };

    let worker_stop = Arc::clone(&stop_flag);
    let worker_events = Arc::clone(&events);
    let worker_dir = dir.clone();
    let handle = std::thread::spawn(move || {
        while !worker_stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }
        worker_events.lock().push("worker_saw_stop".into());
        std::thread::sleep(Duration::from_millis(25));
        // Must still be 'dang_chay' until worker finishes and shutdown runs recover_interrupted_runs
        let conn = crate::state_db::open(&worker_dir).unwrap();
        let st: String = conn
            .query_row(
                "SELECT status FROM runs WHERE id = ?1",
                rusqlite::params![run_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(st, "dang_chay", "recover_interrupted_runs must wait for engine_thread");
        worker_events.lock().push("worker_finished".into());
    });
    *state.engine_thread.lock() = Some(handle);

    let quit_events = Arc::clone(&events);
    shutdown_app_state_with(&state, true, Duration::from_secs(2), move || {
        quit_events.lock().push("quit_sidecar".into());
    });

    assert!(state.exit_requested.load(Ordering::SeqCst));
    assert!(stop_flag.load(Ordering::SeqCst));
    assert!(state.engine_thread.lock().is_none());
    assert_eq!(
        *events.lock(),
        vec![
            "worker_saw_stop".to_string(),
            "worker_finished".to_string(),
            "quit_sidecar".to_string()
        ]
    );
    let conn = crate::state_db::open(&dir).unwrap();
    let recovered_status: String = conn
        .query_row(
            "SELECT status FROM runs WHERE id = ?1",
            rusqlite::params![run_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(recovered_status, "gian_doan");

    // Bounded wait: hung thread must not block shutdown beyond wait_timeout
    let (tx_release, rx_release) = std::sync::mpsc::channel::<()>();
    let hung = std::thread::spawn(move || {
        let _ = rx_release.recv_timeout(Duration::from_secs(2));
    });
    *state.engine_thread.lock() = Some(hung);
    let t0 = Instant::now();
    shutdown_app_state_with(&state, true, Duration::from_millis(50), || {});
    assert!(
        t0.elapsed() < Duration::from_millis(500),
        "shutdown wait on hung engine_thread must be bounded"
    );
    let _ = tx_release.send(());

    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_try_begin_run_and_update_restart_reject_when_exit_requested() {
    let dir = temp_proj("begin_run_exit_guard");
    let state = AppState {
        settings: SettingsStore::with_path(dir.join("settings.json")),
        settings_cache: parking_lot::RwLock::new(Settings::default()),
        project: parking_lot::RwLock::new(Some(dir.clone())),
        last_probed_dir: parking_lot::RwLock::new(None),
        approved_export_paths: parking_lot::RwLock::new(HashSet::new()),
        running: AtomicBool::new(false),
        exit_requested: AtomicBool::new(true),
        active_run_id: AtomicI64::new(0),
        stop_flag: parking_lot::RwLock::new(None),
        engine_thread: parking_lot::Mutex::new(None),
    };

    let err = try_begin_run(&state).unwrap_err();
    assert_eq!(err.code, "APP_EXITING");
    assert!(
        !state.running.load(Ordering::SeqCst),
        "running must remain false when exit_requested is true"
    );

    // If a run is already stopping during exit, try_begin_run refuses without clearing running
    state.running.store(true, Ordering::SeqCst);
    let exiting_while_running = try_begin_run(&state).unwrap_err();
    assert_eq!(exiting_while_running.code, "APP_EXITING");
    assert!(state.running.load(Ordering::SeqCst));

    // If exit_requested was already true, finish_app_update_restart_with rejects without restarting or clearing exit_requested
    let mut stopped = false;
    let mut restarted = false;
    let already_exiting_err = finish_app_update_restart_with(
        &state,
        || stopped = true,
        || restarted = true,
    )
    .unwrap_err();
    assert_eq!(already_exiting_err.code, "APP_EXITING");
    assert!(state.exit_requested.load(Ordering::SeqCst));
    assert!(!stopped);
    assert!(!restarted);

    state.running.store(false, Ordering::SeqCst);
    let already_exiting_idle_err = finish_app_update_restart_with(
        &state,
        || stopped = true,
        || restarted = true,
    )
    .unwrap_err();
    assert_eq!(already_exiting_idle_err.code, "APP_EXITING");
    assert!(state.exit_requested.load(Ordering::SeqCst));
    assert!(!stopped);
    assert!(!restarted);

    // When not exiting, try_begin_run sets running=true and blocks concurrent runs & update restart
    state.exit_requested.store(false, Ordering::SeqCst);
    try_begin_run(&state).expect("try_begin_run succeeds when idle and not exiting");
    assert!(state.running.load(Ordering::SeqCst));

    let dup_err = try_begin_run(&state).unwrap_err();
    assert_eq!(dup_err.code, "RUN_IN_PROGRESS");

    let upd_err = finish_app_update_restart_with(
        &state,
        || stopped = true,
        || restarted = true,
    )
    .unwrap_err();
    assert_eq!(upd_err.code, "RUN_IN_PROGRESS");
    assert!(
        !state.exit_requested.load(Ordering::SeqCst),
        "exit_requested must roll back to false when update restart aborts due to RUN_IN_PROGRESS"
    );
    assert!(!stopped);
    assert!(!restarted);

    // Once idle, finish_app_update_restart_with locks exit_requested=true BEFORE stop_for_quit and restart
    state.running.store(false, Ordering::SeqCst);
    let events = Arc::new(parking_lot::Mutex::new(Vec::<&'static str>::new()));
    let ev_stop = Arc::clone(&events);
    let ev_restart = Arc::clone(&events);
    let ok = finish_app_update_restart_with(
        &state,
        || {
            assert!(
                state.exit_requested.load(Ordering::SeqCst),
                "exit_requested must be true before stop_for_quit"
            );
            let race_err = try_begin_run(&state).unwrap_err();
            assert_eq!(race_err.code, "APP_EXITING");
            assert!(!state.running.load(Ordering::SeqCst));
            ev_stop.lock().push("stop_sidecar");
        },
        || {
            assert!(
                state.exit_requested.load(Ordering::SeqCst),
                "exit_requested must remain true before app.restart"
            );
            ev_restart.lock().push("restart_app");
        },
    )
    .expect("update restart succeeds when idle");
    assert!(ok);
    assert!(state.exit_requested.load(Ordering::SeqCst));
    assert_eq!(*events.lock(), vec!["stop_sidecar", "restart_app"]);

    let post_upd_err = try_begin_run(&state).unwrap_err();
    assert_eq!(post_upd_err.code, "APP_EXITING");
    assert!(!state.running.load(Ordering::SeqCst));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_try_begin_run_and_update_restart_are_mutually_exclusive_under_race() {
    let dir = temp_proj("begin_run_update_race");
    let state = Arc::new(AppState {
        settings: SettingsStore::with_path(dir.join("settings.json")),
        settings_cache: parking_lot::RwLock::new(Settings::default()),
        project: parking_lot::RwLock::new(Some(dir.clone())),
        last_probed_dir: parking_lot::RwLock::new(None),
        approved_export_paths: parking_lot::RwLock::new(HashSet::new()),
        running: AtomicBool::new(false),
        exit_requested: AtomicBool::new(false),
        active_run_id: AtomicI64::new(0),
        stop_flag: parking_lot::RwLock::new(None),
        engine_thread: parking_lot::Mutex::new(None),
    });

    for _ in 0..64 {
        state.running.store(false, Ordering::SeqCst);
        state.exit_requested.store(false, Ordering::SeqCst);
        let barrier = Arc::new(std::sync::Barrier::new(2));

        let s1 = Arc::clone(&state);
        let b1 = Arc::clone(&barrier);
        let t_run = std::thread::spawn(move || {
            b1.wait();
            try_begin_run(&s1)
        });

        let s2 = Arc::clone(&state);
        let b2 = Arc::clone(&barrier);
        let t_upd = std::thread::spawn(move || {
            b2.wait();
            begin_app_update_restart(&s2)
        });

        let run_res = t_run.join().unwrap();
        let upd_res = t_upd.join().unwrap();

        assert!(
            !(run_res.is_ok() && upd_res.is_ok()),
            "try_begin_run and begin_app_update_restart must never both succeed"
        );
        if upd_res.is_ok() {
            assert!(state.exit_requested.load(Ordering::SeqCst));
            assert!(!state.running.load(Ordering::SeqCst));
        }
        if run_res.is_ok() {
            assert!(state.running.load(Ordering::SeqCst));
            assert!(!state.exit_requested.load(Ordering::SeqCst));
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_validate_settings_concurrency_enforces_muse_account_limit() {
    // Gateway: cho phép 1..=MAX_CONCURRENCY bất kể số tài khoản Muse = 0
    let gw_ok = Settings {
        video_source: "gateway".into(),
        concurrency: 8,
        ..Settings::default()
    };
    assert!(validate_settings_concurrency(&gw_ok, 0).is_ok());

    let gw_zero = Settings {
        video_source: "gateway".into(),
        concurrency: 0,
        ..Settings::default()
    };
    assert_eq!(
        validate_settings_concurrency(&gw_zero, 5).unwrap_err().code,
        "INVALID_CONCURRENCY"
    );

    let gw_too_high = Settings {
        video_source: "gateway".into(),
        concurrency: settings::MAX_CONCURRENCY + 1,
        ..Settings::default()
    };
    assert_eq!(
        validate_settings_concurrency(&gw_too_high, 99).unwrap_err().code,
        "INVALID_CONCURRENCY"
    );

    // Muse nội bộ: 0 tài khoản -> cho phép 1 (để lưu cấu hình trước khi đăng nhập), chặn >= 2
    let internal_1 = Settings {
        video_source: "internal".into(),
        concurrency: 1,
        ..Settings::default()
    };
    assert!(validate_settings_concurrency(&internal_1, 0).is_ok());
    assert!(validate_settings_concurrency(&internal_1, 1).is_ok());

    let internal_2 = Settings {
        video_source: "internal".into(),
        concurrency: 2,
        ..Settings::default()
    };
    assert_eq!(
        validate_settings_concurrency(&internal_2, 0).unwrap_err().code,
        "CONCURRENCY_EXCEEDS_ACCOUNTS"
    );
    assert_eq!(
        validate_settings_concurrency(&internal_2, 1).unwrap_err().code,
        "CONCURRENCY_EXCEEDS_ACCOUNTS"
    );
    assert!(validate_settings_concurrency(&internal_2, 2).is_ok());

    // 5 tài khoản Muse -> cho phép 5 luồng, chặn 6 luồng
    let internal_5 = Settings {
        video_source: "internal".into(),
        concurrency: 5,
        ..Settings::default()
    };
    assert!(validate_settings_concurrency(&internal_5, 5).is_ok());
    let internal_6 = Settings {
        video_source: "internal".into(),
        concurrency: 6,
        ..Settings::default()
    };
    assert_eq!(
        validate_settings_concurrency(&internal_6, 5).unwrap_err().code,
        "CONCURRENCY_EXCEEDS_ACCOUNTS"
    );
}
