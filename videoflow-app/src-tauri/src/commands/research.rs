use super::*;
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Write;

fn invalid(message: &str) -> AppError {
    AppError::new("RESEARCH_INVALID", message)
}

fn provider(cfg: &Settings) -> AppResult<Value> {
    if cfg.prompt_source == "internal" {
        return Ok(
            json!({"source":"internal","base_url":"","model":"muse-chat","timeout_s":cfg.prompt_timeout_s}),
        );
    }
    settings::prompt_origin(&cfg.endpoint_prompt_base_url)?;
    if cfg.endpoint_prompt_model.is_empty() {
        return Err(invalid("Hãy chọn model trong Cài đặt → Endpoint prompt."));
    }
    Ok(
        json!({"source":"gateway","base_url":cfg.endpoint_prompt_base_url,"model":cfg.endpoint_prompt_model,
        "timeout_s":cfg.prompt_timeout_s, "api_key":settings::prompt_key(&cfg.endpoint_prompt_base_url).unwrap_or_default()}),
    )
}

pub(super) fn test_prompt(
    cfg: &Settings,
    source: Option<&str>,
    base: Option<&str>,
    model: Option<&str>,
    draft_key: Option<&str>,
) -> AppResult<(bool, u64, String)> {
    let (base, key, model) = match source.unwrap_or(&cfg.prompt_source) {
        "internal" => {
            let session = crate::muse_sidecar::ensure_running(cfg)?;
            (session.v1_base(), session.api_key, "muse-chat")
        }
        "gateway" => {
            let base = base
                .unwrap_or(&cfg.endpoint_prompt_base_url)
                .trim()
                .trim_end_matches('/');
            settings::prompt_origin(base)?;
            let key = draft_key
                .map(str::to_owned)
                .or_else(|| settings::prompt_key(base))
                .unwrap_or_default();
            (
                base.to_owned(),
                key,
                model.unwrap_or(&cfg.endpoint_prompt_model).trim(),
            )
        }
        _ => return Err(invalid("Nguồn prompt không hợp lệ.")),
    };
    if model.is_empty() {
        return Err(invalid("Chưa nhập model."));
    }
    settings::register_secret_for_redaction(&key);
    let start = Instant::now();
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(cfg.prompt_timeout_s.clamp(5, 3600)))
        .build()
        .map_err(|_| invalid("Không khởi tạo được HTTP client."))?;
    let endpoint = if base.ends_with("/chat/completions") { base } else { format!("{base}/chat/completions") };
    let mut request = client
        .post(endpoint)
        .json(&json!({
            "model":model,"stream":false,"messages":[{"role":"user","content":"Reply OK."}]
        }));
    if !key.is_empty() {
        request = request.bearer_auth(key);
    }
    let response = request
        .send()
        .map_err(|_| invalid("Không kết nối được endpoint prompt."))?;
    let status = response.status();
    if !status.is_success() {
        return Err(invalid(&format!("Endpoint prompt trả HTTP {status}.")));
    }
    let body: Value = response
        .json()
        .map_err(|_| invalid("Phản hồi prompt không phải JSON."))?;
    if body
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .is_empty()
    {
        return Err(invalid("Model không trả nội dung."));
    }
    Ok((
        true,
        start.elapsed().as_millis() as u64,
        format!("{model} đã trả lời."),
    ))
}

#[tauri::command]
pub fn research_open_url(url: String) -> AppResult<()> {
    let target = reqwest::Url::parse(&url).map_err(|_|invalid("URL không hợp lệ."))?;
    if !matches!(target.scheme(),"http"|"https") || target.host_str().is_none()
        || !target.username().is_empty() || target.password().is_some() {
        return Err(invalid("Chỉ mở liên kết HTTP(S)."));
    }
    std::process::Command::new("explorer.exe")
        .arg(target.as_str()).spawn().map_err(|e|AppError::new("OPEN_URL",e.to_string()))?;
    Ok(())
}

#[tauri::command]
pub async fn research(
    action: String,
    mut payload: Value,
    app: AppHandle,
    state: State<'_, AppState>,
) -> AppResult<Value> {
    let cfg = state.settings_cache.read().clone();
    tauri::async_runtime::spawn_blocking(move || {
        if !payload.is_object() || payload.get("_provider").is_some() {
            return Err(invalid("Payload hoặc nguồn prompt không hợp lệ."));
        }
        let session = crate::muse_sidecar::ensure_running(&cfg)?;
        match action.as_str() {
            "develop_start" => payload["_provider"] = provider(&cfg)?,
            "batch_resume" | "develop_repair" | "variant_edit" => {
                let batch = post(&session, "batch_get", &json!({"batch_id":payload["batch_id"]}))?;
                let mut saved = batch.pointer("/batch/provider").or_else(||batch.get("provider"))
                    .cloned().ok_or_else(||invalid("Thiếu cấu hình đã lưu của lượt triển khai."))?;
                if saved["source"] == "gateway" {
                    let base = saved["base_url"].as_str().ok_or_else(||invalid("Thiếu endpoint đã lưu."))?;
                    let key = settings::prompt_key(base).ok_or_else(||invalid("Thiếu API key cho endpoint đã lưu của lượt triển khai."))?;
                    saved["api_key"] = json!(key);
                }
                payload["_provider"] = saved;
            }
            "search_start" | "search_list" | "search_get" | "search_resume" | "search_stop" |
            "search_sources" | "set_sources" | "idea_get" | "prepare_ideas" | "idea_update" |
            "select_idea" | "batch_get" | "batch_stop" => {}
            _ => return Err(invalid("Thao tác Reels không hợp lệ.")),
        }
        let mut result = post(&session,&action,&payload)?;
        attach_assets(&app, &mut result)?;
        if !matches!(action.as_str(),"search_get"|"search_list"|"search_sources"|"idea_get"|"batch_get") {
            let _ = app.emit("research:changed",json!({"action":action, "session_id":result.get("session_id").or_else(||payload.get("session_id")), "entity_id":result.get("batch_id").or_else(||payload.get("batch_id")), "seq":result.get("seq")}));
        }
        Ok(result)
    }).await.map_err(|e| invalid(&e.to_string()))?
}

fn attach_assets(app: &AppHandle, value: &mut Value) -> AppResult<()> {
    match value {
        Value::Array(items) => {
            for item in items {
                attach_assets(app, item)?;
            }
        }
        Value::Object(fields) => {
            for child in fields.values_mut() {
                attach_assets(app, child)?;
            }
            if let (Some(hash), Some(mime)) = (
                fields.get("asset_hash").and_then(Value::as_str),
                fields.get("asset_mime").and_then(Value::as_str),
            ) {
                let ext = match mime {
                    "image/jpeg" => "jpg",
                    "image/png" => "png",
                    "image/webp" => "webp",
                    _ => return Ok(()),
                };
                if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                    let path = settings::app_data_dir()
                        .join("research")
                        .join("assets")
                        .join(format!("{hash}.{ext}"));
                    if path.is_file() {
                        app.asset_protocol_scope()
                            .allow_file(&path)
                            .map_err(|e| invalid(&e.to_string()))?;
                        fields.insert("asset_path".into(), json!(path));
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn post(
    session: &crate::muse_sidecar::SidecarSession,
    action: &str,
    payload: &Value,
) -> AppResult<Value> {
    let mut result = crate::muse_sidecar::admin_send_timeout(
        session.port,
        &session.api_key,
        "POST",
        &format!("/internal/research/{action}"),
        Some(payload),
        60,
    )?;
    if result["ok"] != true || result.get("data").is_none() {
        return Err(invalid("Phản hồi Reels không hợp lệ."));
    }
    Ok(result["data"].take())
}

#[derive(Deserialize)]
struct TransferRow {
    ordinal: u32,
    image_prompt: String,
    video_prompt: String,
}
#[derive(Deserialize)]
struct Prepared {
    image_b64: String,
    image_sha256: String,
    rows: Vec<TransferRow>,
}

fn write_rows(project: &Path, ordinals: &[u32], data: Prepared) -> AppResult<usize> {
    if data
        .rows
        .iter()
        .map(|r| r.ordinal)
        .ne(ordinals.iter().copied())
        || data
            .rows
            .iter()
            .any(|r| r.image_prompt.trim().is_empty() || r.video_prompt.trim().is_empty())
    {
        return Err(invalid("Dữ liệu bàn giao không khớp các dòng đã chọn."));
    }
    if data.image_b64.len() > 32 * 1024 * 1024 {
        return Err(invalid("Ảnh quá lớn."));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.image_b64)
        .map_err(|_| invalid("Ảnh không hợp lệ."))?;
    let hash = format!("{:x}", Sha256::digest(&bytes));
    if hash != data.image_sha256 {
        return Err(invalid("Hash ảnh không khớp."));
    }
    let ext = match image::guess_format(&bytes) {
        Ok(image::ImageFormat::Png) => "png",
        Ok(image::ImageFormat::Jpeg) => "jpg",
        Ok(image::ImageFormat::WebP) => "webp",
        _ => return Err(invalid("Định dạng ảnh không hợp lệ.")),
    };
    let name = format!("reel-{hash}.{ext}");
    let input = project.join("input");
    let dest = input.join(&name);
    let io = |e: std::io::Error| AppError::new("RESEARCH_IO", e.to_string());
    if !dest.exists() {
        let mut temp = tempfile::NamedTempFile::new_in(input).map_err(io)?;
        temp.write_all(&bytes).map_err(io)?;
        temp.as_file().sync_all().map_err(io)?;
        if let Err(e) = temp.persist_noclobber(&dest) {
            if e.error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(io(e.error));
            }
        }
    }
    if std::fs::read(dest).map_err(io)? != bytes {
        return Err(invalid("Ảnh trong project đã thay đổi."));
    }
    let rows = data
        .rows
        .into_iter()
        .map(|r| (name.clone(), r.image_prompt, r.video_prompt))
        .collect::<Vec<_>>();
    prompts_csv::append_rows(&project.join("prompts.csv"), &rows)
}

#[tauri::command]
pub async fn research_transfer(
    batch_id: String,
    mut ordinals: Vec<u32>,
    request_id: String,
    expected_project: String,
    app: AppHandle,
) -> AppResult<Value> {
    tauri::async_runtime::spawn_blocking(move || {
        let state=app.state::<AppState>();
        ensure_not_running(&state)?;
        ordinals.sort_unstable();
        if ordinals.is_empty() || ordinals.len()>10 || ordinals.iter().any(|i| !(1..=10).contains(i)) || ordinals.windows(2).any(|v|v[0]==v[1]) {return Err(invalid("Danh sách dòng không hợp lệ."));}
        let project=project_path(&state)?;
        if project!=Path::new(&expected_project) {return Err(invalid("Project đã thay đổi. Vui lòng xác nhận lại."));}
        let cfg=state.settings_cache.read().clone();
        let session=crate::muse_sidecar::ensure_running(&cfg)?;
        let mut payload=json!({"batch_id":batch_id,"ordinals":ordinals,"request_id":request_id,"project":project});
        let prepared=post(&session,"transfer_prepare",&payload)?;
        if prepared["status"]=="committed" || prepared["committed"]==true {return Ok(prepared);}
        let data=serde_json::from_value(prepared).map_err(|_| invalid("Phản hồi bàn giao thiếu dữ liệu."))?;
        let gate = PROJECT_WRITE_GATE.lock();
        let current=state.project.read();
        if current.as_ref()!=Some(&project) {return Err(invalid("Project đã thay đổi."));}
        ensure_not_running(&state)?;
        let added=write_rows(&project,&ordinals,data)?;
        drop(current);
        drop(gate);
        let _ = app.emit("external_files_changed",json!({"path":project}));
        payload["added"]=json!(added);
        payload["existing"]=json!(ordinals.len()-added);
        let result=post(&session,"transfer_commit",&payload)?;
        let _=app.emit("research:changed",json!({"action":"transfer_commit","batch_id":batch_id}));
        Ok(result)
    }).await.map_err(|e|invalid(&e.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(rows: &[u32]) -> Prepared {
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(4, 4)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let bytes = encoded.into_inner();
        Prepared {
            image_b64: base64::engine::general_purpose::STANDARD.encode(&bytes),
            image_sha256: format!("{:x}", Sha256::digest(bytes)),
            rows: rows
                .iter()
                .map(|&ordinal| TransferRow {
                    ordinal,
                    image_prompt: format!("Ảnh {ordinal}"),
                    video_prompt: format!("Video {ordinal}"),
                })
                .collect(),
        }
    }

    #[test]
    fn transfer_reuses_image_and_existing_rows_after_interrupted_commit() {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir(project.path().join("input")).unwrap();
        assert_eq!(write_rows(project.path(), &[1], sample(&[1])).unwrap(), 1);
        assert_eq!(
            write_rows(project.path(), &[1, 3], sample(&[1, 3])).unwrap(),
            1
        );
        assert_eq!(
            write_rows(project.path(), &[1, 3], sample(&[1, 3])).unwrap(),
            0
        );
        let rows = prompts_csv::read_all(&project.path().join("prompts.csv")).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].anh_tham_chieu, rows[1].anh_tham_chieu);
        assert_eq!(rows[1].promt_tao_video, "Video 3");
        assert_eq!(
            std::fs::read_dir(project.path().join("input"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn mismatched_hash_or_ordinals_prevent_writes() {
        let project = tempfile::tempdir().unwrap();
        let mut data = sample(&[1]);
        data.image_sha256 = "0".repeat(64);
        assert!(write_rows(project.path(), &[1], data).is_err());
        assert!(write_rows(project.path(), &[1], sample(&[2])).is_err());
        assert_eq!(std::fs::read_dir(project.path()).unwrap().count(), 0);
    }

    #[test]
    fn existing_install_gets_research_defaults_and_separate_prompt_endpoint() {
        let settings: Settings =
            serde_json::from_value(json!({"video_source":"internal"})).unwrap();
        assert_eq!(
            (settings.reels_min_likes, settings.reels_min_views),
            (Some(500), Some(30_000))
        );
        assert_eq!(settings.prompt_source, "gateway");
        assert!(settings.endpoint_prompt_base_url.is_empty());
        assert_eq!(
            settings::prompt_origin("https://example.org:443/v1").unwrap(),
            "https://example.org"
        );
        assert!(settings::prompt_origin("https://key@example.org/v1").is_err());
    }
}
