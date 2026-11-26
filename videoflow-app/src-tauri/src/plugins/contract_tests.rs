//! Runs the public contract fixture through the normal worker/SQLite pipeline.
use super::{client, host, routing};
use crate::{engine, prompts_csv, settings::Settings};
use base64::Engine;
use serde_json::{json, Value};
use std::{
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    process::Command,
    sync::{atomic::AtomicBool, Arc},
};

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        for id in ["default", "second"] {
            let _ = host::stop("demo", id);
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn installed_provider_roundtrip() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/plugin-contract/v1.2");
    let root = std::env::temp_dir().join(format!(
        "vf-contract-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_micros()
    ));
    let _guard = Fixture(root.clone());
    let package = root.join("resources/plugins/demo");
    fs::create_dir_all(&package).unwrap();
    // A --copies venv installs a real local entry executable. No Python packages
    // or network access are needed by this fixture.
    let python = if cfg!(windows) { "python" } else { "python3" };
    assert!(Command::new(python)
        .args(["-m", "venv", "--copies", "--without-pip"])
        .arg(package.join("runtime"))
        .status()
        .unwrap()
        .success());
    let sample: Value =
        serde_json::from_slice(&fs::read(source.join("responses.json")).unwrap()).unwrap();
    for name in ["responses.json", "demo.py", "sample.mp4"] {
        fs::copy(source.join(name), package.join(name)).unwrap();
    }
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(source.join("manifest.json")).unwrap()).unwrap();
    manifest["entry"]["exe"] = json!(if cfg!(windows) {
        "runtime/Scripts/python.exe"
    } else {
        "runtime/bin/python"
    });
    manifest["entry"]["args"]
        .as_array_mut()
        .unwrap()
        .insert(0, json!(package.join("demo.py")));
    manifest["config_schema"]["properties"]["image_model"] =
        json!({"type":"string","default":"i1"});
    manifest["config_schema"]["properties"]["drop_response"] =
        json!({"type":"boolean","default":false});
    fs::write(
        package.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    let app_data = root.join("app-data");
    host::initialize(root.join("resources"), app_data.clone());
    host::set_enabled("demo", "default", true).unwrap();
    let owner = "plugin:demo:default";
    let session = host::session(owner).unwrap();
    let wrong = reqwest::blocking::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("{}/healthz", session.origin))
        .bearer_auth("incorrect")
        .send()
        .unwrap();
    assert_eq!(wrong.status().as_u16(), 401);

    let project = root.join("project");
    for name in ["input", "image_out", "video_out"] {
        fs::create_dir_all(project.join(name)).unwrap();
    }
    let png = base64::engine::general_purpose::STANDARD
        .decode(
            sample["image_lookup_200"]["data"][0]["b64_json"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
    let input = project.join("input/ref.png");
    fs::write(&input, &png).unwrap();
    let cfg = Settings {
        image_source: owner.into(),
        video_source: owner.into(),
        video_size: "1280x720".into(),
        concurrency: 1,
        ..Default::default()
    };
    prompts_csv::write_all(
        &project.join("prompts.csv"),
        &[prompts_csv::Row {
            row_no: 2,
            anh_tham_chieu: "ref.png".into(),
            promt_tao_image: "draw".into(),
            promt_tao_video: "animate".into(),
            trang_thai: "Đang chờ".into(),
            ..Default::default()
        }],
    )
    .unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let run = engine::run_with_emitter_and_hook(
        &|_, _| {},
        None,
        &cfg,
        &project,
        engine::EngineConfig {
            kind: "chung",
            ..Default::default()
        },
        &stop,
    )
    .unwrap();
    let rows = prompts_csv::read_all(&project.join("prompts.csv")).unwrap();
    assert_eq!((run.done, run.errors), (1, 0), "{rows:?}");
    let mp4 = fs::read(project.join("video_out").join(&rows[0].video_da_tao)).unwrap();
    assert_eq!(mp4, fs::read(source.join("sample.mp4")).unwrap());

    // Both independent projects start at run #1, job #1. Losing each POST
    // response in the second project must still commit the right artifacts.
    host::save_config(
        "demo",
        "default",
        json!({"video_model":"m1","image_model":"i1","drop_response":true}),
        Default::default(),
    )
    .unwrap();
    let project2 = root.join("project2");
    for name in ["input", "image_out", "video_out"] {
        fs::create_dir_all(project2.join(name)).unwrap();
    }
    fs::copy(&input, project2.join("input/ref.png")).unwrap();
    let next_row = prompts_csv::Row {
        row_no: 2,
        anh_tham_chieu: "ref.png".into(),
        promt_tao_image: "draw".into(),
        promt_tao_video: "animate".into(),
        trang_thai: "Đang chờ".into(),
        ..Default::default()
    };
    prompts_csv::write_all(&project2.join("prompts.csv"), &[next_row]).unwrap();
    let second_run = engine::run_with_emitter_and_hook(
        &|_, _| {},
        None,
        &cfg,
        &project2,
        engine::EngineConfig {
            kind: "chung",
            ..Default::default()
        },
        &stop,
    )
    .unwrap();
    assert_eq!((second_run.done, second_run.errors), (1, 0));
    let provider_db =
        rusqlite::Connection::open(app_data.join("plugins-data/demo/default/operations.db"))
            .unwrap();
    let generated: i64 = provider_db
        .query_row(
            "SELECT count(*) FROM operations WHERE fingerprint IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        generated, 4,
        "independent project UUIDs survive dropped responses"
    );
    drop(provider_db);

    let key = "1baac87a-f5d3-46a6-9a12-05be8679286f";
    assert_eq!(client::cancel(owner, key).unwrap()["status"], "cancelled");
    assert_eq!(
        client::submit_video(&cfg, owner, key, "m1", &input, "late")
            .unwrap_err()
            .http,
        Some(409)
    );
    let port = reqwest::Url::parse(&session.origin)
        .unwrap()
        .port()
        .unwrap();
    host::stop("demo", "default").unwrap();
    assert!(
        host::session(owner).is_err(),
        "a stopped instance must wait for explicit start"
    );
    // Windows may already reserve this exact endpoint in TIME_WAIT after shutdown.
    let _occupied = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => Some(listener),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => None,
        Err(e) => panic!("could not reserve the previous port: {e}"),
    };
    host::start("demo", "default").unwrap();
    let new_session = host::session(owner).unwrap();
    assert_ne!(session.origin, new_session.origin);
    assert_eq!(client::lookup(owner, key).unwrap()["status"], "cancelled");

    host::add_instance("demo", "second").unwrap();
    host::set_enabled("demo", "second", true).unwrap();
    let first = routing::acquire(owner, "video", &stop).unwrap();
    let second = routing::acquire("plugin-pool:demo", "image", &stop).unwrap();
    assert_ne!(first.owner, second.owner);
    assert_ne!(session.token, host::session(&second.owner).unwrap().token);
}
