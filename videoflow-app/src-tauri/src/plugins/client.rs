//! HTTP client for the versioned provider contract. Mutating requests are never
//! automatically replayed: the engine owns the persisted operation key.
use super::host;
use crate::adapters::{self, ApiFail, ApiFailKind};
use crate::settings::Settings;
use reqwest::{
    blocking::{multipart, Client, RequestBuilder},
    Method,
};
use serde_json::{json, Value};
use std::{path::Path, time::Duration};

#[derive(Debug, Clone)]
pub struct PluginError {
    pub http: Option<u16>,
    pub kind: Box<str>,
    pub scope: Box<str>,
    pub code: String,
    pub message: String,
    pub recoverable_media_id: Option<Box<str>>,
    pub retryable: bool,
    pub retry_after_s: Option<u64>,
}

impl PluginError {
    pub fn local(code: &str, message: impl Into<String>) -> Self {
        Self {
            http: None,
            kind: "business".into(),
            scope: "operation".into(),
            code: code.into(),
            message: message.into(),
            recoverable_media_id: None,
            retryable: false,
            retry_after_s: None,
        }
    }

    pub fn from_error(error: &Value, http: Option<u16>) -> Self {
        let field = |key: &str| error.get(key).and_then(Value::as_str);
        let kind = field("kind").unwrap_or("business");
        Self {
            http,
            kind: kind.into(),
            scope: field("scope").unwrap_or("operation").into(),
            code: field("code").unwrap_or("PLUGIN_UNAVAILABLE").into(),
            message: crate::settings::redact_secrets(
                field("message").unwrap_or("Phản hồi plugin không hợp lệ"),
            ),
            recoverable_media_id: field("recoverable_media_id").map(Into::into),
            retryable: kind == "transient" && error["retryable"].as_bool() == Some(true),
            retry_after_s: error["retry_after_s"].as_u64(),
        }
    }

    pub fn as_api_fail(&self) -> ApiFail {
        // The legacy pipeline treats HetQuota as a global stop. Plugin quota
        // is scoped by routing::block and must never stop unrelated owners.
        let kind = if self.retryable {
            ApiFailKind::TamThoi {
                retry_after_s: self.retry_after_s,
            }
        } else {
            ApiFailKind::NghiepVu
        };
        ApiFail {
            kind,
            http_status: self.http,
            code: self.code.clone(),
            message: self.message.clone(),
        }
    }
}

fn segment(s: &str) -> Result<String, PluginError> {
    adapters::sanitize_url_segment(s, false).map_err(|e| PluginError::local(&e.code, e.message))
}

fn request(
    owner: &str,
    method: Method,
    path: &str,
    seconds: u64,
) -> Result<RequestBuilder, PluginError> {
    let s =
        host::session(owner).map_err(|e| PluginError::local("PLUGIN_UNAVAILABLE", e.message))?;
    let client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(seconds.max(1)))
        .build()
        .map_err(|_| PluginError::local("PLUGIN_UNAVAILABLE", "Không tạo được HTTP client"))?;
    Ok(client
        .request(method, format!("{}{path}", s.origin))
        .bearer_auth(s.token))
}

fn response(req: RequestBuilder) -> Result<Value, PluginError> {
    let r = req.send().map_err(|_| PluginError {
        kind: "transient".into(),
        ..PluginError::local(
            "PLUGIN_UNAVAILABLE",
            "Mất kết nối plugin; cần tra cứu trạng thái thao tác",
        )
    })?;
    let status = r.status().as_u16();
    let body = r.json::<Value>().unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        return Err(PluginError::from_error(&body["error"], Some(status)));
    }
    if !body.is_object() {
        return Err(PluginError::local(
            "PLUGIN_BAD_RESPONSE",
            "Plugin trả dữ liệu không hợp lệ",
        ));
    }
    Ok(body)
}

pub fn get(owner: &str, path: &str) -> Result<Value, PluginError> {
    response(request(owner, Method::GET, path, 30)?)
}
pub fn lookup(owner: &str, key: &str) -> Result<Value, PluginError> {
    get(
        owner,
        &format!("/vf-plugin/v1/operations/{}", segment(key)?),
    )
}
pub fn cancel(owner: &str, key: &str) -> Result<Value, PluginError> {
    response(request(
        owner,
        Method::DELETE,
        &format!("/vf-plugin/v1/operations/{}", segment(key)?),
        30,
    )?)
}
pub fn poll(owner: &str, id: &str) -> Result<Value, PluginError> {
    get(owner, &format!("/v1/videos/{}", segment(id)?))
}
pub fn media_url(owner: &str, name: &str) -> Result<String, PluginError> {
    let name = adapters::sanitize_url_segment(name, true)
        .map_err(|e| PluginError::local(&e.code, e.message))?;
    let s =
        host::session(owner).map_err(|e| PluginError::local("PLUGIN_UNAVAILABLE", e.message))?;
    Ok(format!("{}/v1/media/{name}", s.origin))
}

pub fn submit_image(
    cfg: &Settings,
    owner: &str,
    key: &str,
    model: &str,
    image: &Path,
    prompt: &str,
) -> Result<Value, PluginError> {
    let bytes = std::fs::read(image)
        .map_err(|_| PluginError::local("IMAGE_READ_FAIL", "Không đọc được ảnh"))?;
    let mime = adapters::detect_image_mime(&bytes, image);
    let part = multipart::Part::bytes(bytes)
        .file_name("image")
        .mime_str(mime)
        .map_err(|_| PluginError::local("BAD_MIME", "Định dạng ảnh không hợp lệ"))?;
    let form = multipart::Form::new()
        .text("model", model.to_owned())
        .text("prompt", prompt.to_owned())
        .text("n", "1")
        .text("response_format", "b64_json")
        .part("image", part);
    response(
        request(owner, Method::POST, "/v1/images/edits", cfg.image_timeout_s)?
            .header("Idempotency-Key", key)
            .multipart(form),
    )
}
pub fn submit_video(
    cfg: &Settings,
    owner: &str,
    key: &str,
    model: &str,
    image: &Path,
    prompt: &str,
) -> Result<Value, PluginError> {
    let bytes = std::fs::read(image)
        .map_err(|_| PluginError::local("IMAGE_READ_FAIL", "Không đọc được ảnh"))?;
    let data = adapters::build_video_image_data_url(&bytes, image, 10 * 1024 * 1024)
        .map_err(|e| PluginError::local(&e.code, e.message))?;
    let payload = json!({"model":model,"prompt":prompt,"size":cfg.video_size,"seconds":cfg.video_seconds,"image":data,"input_reference":data});
    response(
        request(
            owner,
            Method::POST,
            "/v1/videos",
            cfg.video_create_timeout_s,
        )?
        .header("Idempotency-Key", key)
        .json(&payload),
    )
}

pub fn action(owner: &str, id: &str, payload: &Value) -> Result<Value, PluginError> {
    response(
        request(
            owner,
            Method::POST,
            &format!("/vf-plugin/v1/actions/{}", segment(id)?),
            60,
        )?
        .json(payload),
    )
}
