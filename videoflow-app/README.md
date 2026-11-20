# VideoFlow Desktop (v1.5.2 — Full Option)

Ứng dụng Desktop tự động hóa quy trình **Tạo Ảnh → Tạo Video (Local-Only)** triển khai đầy đủ theo **Bản thiết kế v1.3**, **Phụ lục chốt v1.3.1** và **Kế hoạch nâng cấp v1.5.x**, xây dựng trên nền tảng **Tauri v2 (Rust)** + **Svelte 4 / TypeScript**.

**Từ v1.4.0 – v1.5.2:** tích hợp engine nội bộ **VideoFlow Muse** (đóng gói kèm app) — quản lý pool tài khoản/cookie muse.ai với **hồ sơ trình duyệt (profile) và proxy riêng từng tài khoản**, đăng nhập trực tiếp trên trình duyệt có giao diện để tự bắt token, tạo **video** và **ảnh (`muse-image`)**, nối cảnh liên tục (`noi_canh`), tự động ghép video sau batch (`auto_merge` qua FFmpeg), theo dõi hạn mức và **tự cập nhật** qua GitHub Releases.

Phát hành: xem [`docs/RELEASE.md`](docs/RELEASE.md) · Tải bản mới: https://github.com/ngapngap/VideoFlow/releases/latest

## Chạy & Kiểm tra

### Phát triển / Chạy ứng dụng

```bash
cd videoflow-app
npm install          # Cài đặt dependencies frontend (1 lần)
npm run build        # Đóng gói giao diện vào dist/
cd src-tauri
# Lưu ý: Tauri yêu cầu tồn tại file externalBin `binaries/videoflow-muse-x86_64-pc-windows-msvc.exe`
# khi biên dịch (xem sidecar/BUGFIXES.md để build bằng PyInstaller hoặc tạo file stub khi chỉ test UI).
cargo run            # Chạy ứng dụng Desktop
```

*(Lưu ý: ở chế độ `cargo run` debug, Tauri kết nối tới `devUrl http://localhost:1420` — chạy song song `npm run dev` ở cửa sổ terminal khác, hoặc biên dịch bản release độc lập bằng `cargo build --release` trong `src-tauri/` để nhúng sẵn `dist/` vào `target/release/videoflow-desktop.exe`).*

### Bộ kiểm tra chất lượng & E2E tự động

```bash
# Kiểm tra kiểu TypeScript, Unit Tests frontend & đóng gói giao diện
cd videoflow-app
npm run check
npm test
npm run build

# Kiểm tra Linter & Unit Tests cho Sidecar Python
cd sidecar
ruff check .
pytest -q

# Chạy Unit Tests + Live Local-HTTP E2E Suite & Clippy Linter (Rust Backend)
cd ../src-tauri
cargo test
cargo clippy --all-targets -- -D warnings
```

Yêu cầu môi trường: Windows 10/11 (WebView2), Rust stable MSVC (`1.77+`), Visual Studio C++ Build Tools, Node.js `20+`, Python `3.11+` (khi build sidecar).

---

## Kiến trúc & Toàn bộ Tính năng (v1.5.2 — Full Option)

| Thành phần | File nguồn | Mô tả theo thiết kế (v1.3 + Phụ lục v1.3.1 + v1.5.2) |
|---|---|---|
| **Tauri v2 ACL, Asset Protocol & CSP** | `src-tauri/capabilities/default.json`, `tauri.conf.json`, `Cargo.toml` | Khai báo quyền tối giản (`core:default`, `core:event:default`) — toàn bộ thao tác hệ thống đi qua Rust IPC commands; giao thức `asset:` kèm cache-busting theo từng dòng (`rowAssetRevs`) và chính sách CSP bảo mật trên WebView2. |
| **SQLite State DB (Schema v4 + Auto-Cleanup)** | `src-tauri/src/state_db.rs` | 6 bảng nghiệp vụ + `meta`, bật `WAL`, `foreign_keys`, `busy_timeout = 5000ms` trên mọi kết nối; mở nhanh qua `PRAGMA user_version`, tự động migrate `v1 → v4` (`video_task_id`, `video_download_url`, trạng thái `loi_tam_thoi`, bước `auto_merge`), phục hồi lượt gián đoạn (`recover_interrupted_runs`) và tự dọn lịch sử quá hạn (`cleanup_old_history`) bảo toàn 100% `file_refs` & job mới nhất của từng dòng. |
| **Lưu trữ `prompts.csv` & Khóa dòng `row_key`** | `src-tauri/src/prompts_csv.rs`, `csv_writer.rs` | Đọc/ghi chuẩn **UTF-8 có BOM + CRLF (RFC 4180)** đồng bộ qua `ReentrantMutex` (`GLOBAL_QUEUE`) + đổi tên nguyên tử; hỗ trợ nhiều dòng dùng chung một ảnh tham chiếu nhưng khác prompt thông qua khóa dòng `row_key = tên_ảnh#hash(prompt_ảnh + prompt_video)`; hỗ trợ cột `noi_canh` (cột K) để nối cảnh liên tục. |
| **Quản lý Thư mục Dự án** | `src-tauri/src/project.rs` | Chuẩn hóa đường dẫn thống nhất (gỡ `\\?\`, xử lý gốc ổ đĩa `D:\`), kiểm tra cấu trúc 5 mục kèm kiểm định nội dung `prompts.csv`, tự sao lưu & phục hồi `state.db` hỏng (`state.db.corrupt_*`), hỗ trợ **Giữ lại (`giu_lai`)** và **Tạo mới (`ghi_de`)** kèm sao lưu `prompts.backup_YYYYMMDD_HHMMSS_mmm.csv`. |
| **HTTP Adapters & VideoFlow Muse Sidecar** | `src-tauri/src/adapters.rs`, `muse_sidecar.rs`, `sidecar/muse2api/*` | Pool `reqwest::blocking::Client` (luôn `.no_proxy()` cho loopback sidecar), multipart tạo ảnh kèm chuẩn hóa PNG/JPEG, `Idempotency-Key` & `Retry-After` cho HTTP `429`, phân loại chính xác lỗi tạm thời (`LoiTamThoi`) vs lỗi nghiệp vụ/khóa video; sidecar chạy Chromium riêng cho từng tài khoản kèm relay cục bộ cho HTTP proxy có xác thực `user:pass`. |
| **Run Engine (`1..3` dòng + Nối cảnh + Auto-merge)** | `src-tauri/src/engine.rs` | Thực thi song song `1..3` worker threads (tự chuyển về tuần tự khi batch có dòng `noi_canh`), trích frame cuối của cảnh trước bằng FFmpeg để nối cảnh, tự động ghép nối các clip thành phẩm sau batch (`auto_merge_batch`), cấp số phiên bản `_vN` chống ghi đè, bảo toàn `video_task_id` & `video_download_url`, dừng an toàn `< 500ms`. |
| **Dịch vụ Nền & Tích hợp Windows** | `src-tauri/src/runtime_services.rs`, `lib.rs`, `settings.rs` | Khay hệ thống **System Tray**, chắn đóng cửa sổ an toàn khi đang chạy, theo dõi thay đổi thư mục dự án thời gian thực (**Filesystem Watcher**), bộ lập lịch chạy tự động định kỳ (**Scheduler** + cảnh báo `schedule_missed`), thông báo khi hoàn tất và đăng ký khởi động cùng Windows (`HKCU\...\Run`). |
| **Cài đặt & Bảo mật Khóa** | `src-tauri/src/settings.rs` | Lưu/xóa API key trong **Windows Credential Manager**, ghi nguyên tử `settings.json`, chuẩn hóa `.trim()` URL/model, che khóa ngắn an toàn (`mask_api_key`) và tự động lọc che cả khóa thực tế trong Keyring lẫn `Bearer <token>` / `sk-*` trên mọi dòng `logs` (`message` & `detail`). |
| **Giao diện 7 Màn hình** | `src/App.svelte`, `src/screens/*`, `src/lib/*` | 7 màn hình chuyên biệt (**Tổng quan**, **Dòng việc**, **Tài khoản**, **Proxy**, **Lịch sử**, **Thư mục Local**, **Cài đặt**): quản lý pool tài khoản Muse & **Proxy (HTTP / SOCKS5)** tập trung trong `proxies.db` (test kết nối thực tế tới `muse.ai`, đo độ trễ/Exit IP & cảnh báo trùng IP), chế độ xem **Bảng ↔ Lưới thẻ Media**, Modal xem video lớn kèm thông số MP4 & sao chép prompt, Lightbox phóng to/kéo (`25%–400%`), so sánh `v1 vs v2` song song, bộ lọc ngày/loại + xuất `.csv` & `.log`. |
