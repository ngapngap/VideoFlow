# VideoFlow Desktop (v1.4.0 — Full Option)

Ứng dụng Desktop tự động hóa quy trình **Tạo Ảnh → Tạo Video (Local-Only)** triển khai đầy đủ theo **Bản thiết kế v1.3** và **Phụ lục chốt v1.3.1**, xây dựng trên nền tảng **Tauri v2 (Rust)** + **Svelte 4 / TypeScript**.

**Từ v1.4.0:** có thêm engine nội bộ **VideoFlow Muse** (đóng gói kèm app) — quản lý pool cookie muse.ai, tạo **video** và **ảnh bằng model `muse-image`** trực tiếp, theo dõi hạn mức (quota tuần + quota bổ sung); và **tự cập nhật** qua GitHub Releases.

Phát hành: xem [`docs/RELEASE.md`](docs/RELEASE.md) · Tải bản mới: https://github.com/ngapngap/VideoFlow/releases/latest

## Chạy & Kiểm tra

### Phát triển / Chạy ứng dụng

```bash
cd videoflow-app
npm install          # Cài đặt dependencies frontend (1 lần)
npm run build        # Đóng gói giao diện vào dist/
cd src-tauri
cargo run            # Chạy ứng dụng Desktop
```

*(Lưu ý: ở chế độ `cargo run` debug, Tauri kết nối tới `devUrl http://localhost:1420` — chạy song song `npm run dev` ở cửa sổ terminal khác, hoặc biên dịch bản release độc lập bằng `cargo build --release` trong `src-tauri/` để nhúng sẵn `dist/` vào `target/release/videoflow-desktop.exe`).*

### Bộ kiểm tra chất lượng & E2E tự động

```bash
# Kiểm tra kiểu TypeScript & Accessibility (Svelte)
cd videoflow-app
npm run check
npm run build

# Chạy Unit Tests + Live Local-HTTP E2E Suite & Clippy Linter (Rust Backend)
cd src-tauri
cargo test
cargo clippy --all-targets -- -D warnings
```

Yêu cầu môi trường: Windows 10/11 (WebView2), Rust stable MSVC (`1.77+`), Visual Studio C++ Build Tools, Node.js `18+`.

---

## Kiến trúc & Toàn bộ Tính năng (Wave 2 — Full Option)

| Thành phần | File nguồn | Mô tả theo thiết kế (v1.3 + Phụ lục v1.3.1) |
|---|---|---|
| **Tauri v2 ACL, Asset Protocol & CSP** | `src-tauri/capabilities/default.json`, `tauri.conf.json`, `Cargo.toml` | Khai báo quyền tối giản (`core:default`, `core:event:default`), giao thức `asset:` kèm cache-busting xem ảnh/video cục bộ và chính sách CSP bảo mật trên WebView2. |
| **SQLite State DB (Schema v2 + Auto-Cleanup)** | `src-tauri/src/state_db.rs` | 6 bảng nghiệp vụ + `meta`, bật `WAL`, `foreign_keys`, `busy_timeout = 5000ms` trên mọi kết nối; mở nhanh qua `PRAGMA user_version`, nâng cấp `v1 → v2` (`video_task_id`, `video_download_url`), phục hồi lượt gián đoạn (`recover_interrupted_runs`) và tự dọn lịch sử/nhật ký quá hạn (`cleanup_old_history`) bảo toàn 100% `file_refs` & job mới nhất của từng dòng. |
| **Lưu trữ `prompts.csv` Đồng bộ** | `src-tauri/src/prompts_csv.rs`, `csv_writer.rs` | Đọc/ghi chuẩn **UTF-8 có BOM + CRLF (RFC 4180)** đồng bộ qua `ReentrantMutex` (`GLOBAL_QUEUE`) + đổi tên nguyên tử; nới lỏng cột J & bảo toàn cột phụ; chặn khóa trống cột A, path traversal, trùng khóa/trùng `file_stem` không phân biệt hoa/thường trên NTFS; nhập hàng loạt ảnh từ `input/` (`append_rows`) và đổi phiên bản sản phẩm chính (`set_active_media_file`). |
| **Quản lý Thư mục Dự án** | `src-tauri/src/project.rs` | Chuẩn hóa đường dẫn thống nhất (gỡ `\\?\`, xử lý gốc ổ đĩa `D:\`), kiểm tra cấu trúc 5 mục kèm kiểm định nội dung `prompts.csv`, tự sao lưu & phục hồi `state.db` hỏng (`state.db.corrupt_*`), hỗ trợ **Giữ lại (`giu_lai`)** và **Tạo mới (`ghi_de`)** kèm sao lưu `prompts.backup_YYYYMMDD_HHMMSS_mmm.csv`. |
| **HTTP Adapters** | `src-tauri/src/adapters.rs` | Pool `reqwest::blocking::Client` theo timeout, multipart tạo ảnh kèm MIME chuẩn & chuẩn hóa PNG, tự nén JPEG chất lượng 85 khi ảnh `> 10 MB` (kèm ghi log cảnh báo), `Idempotency-Key` & `Retry-After` cho HTTP `429`, chỉ đính `Bearer` cùng origin khi tải video (an toàn với S3/R2 Presigned URL), stream tải `.part` và kiểm tra header MP4 (`ftyp`). |
| **Run Engine Song song (`1..3` dòng)** | `src-tauri/src/engine.rs` | Thực thi song song thực sự `1..3` worker threads (`cfg.concurrency`), bảo toàn `video_task_id` & `video_download_url` không đốt lại quota, cập nhật hash prompt chỉ sau khi bước thực thi thành công, đồng bộ ngược toàn diện cột D/E/G/H/I/J khi mở khóa Excel (TC-44), dừng an toàn `< 500ms`. |
| **Dịch vụ Nền & Tích hợp Windows** | `src-tauri/src/runtime_services.rs`, `lib.rs`, `settings.rs` | Khay hệ thống **System Tray**, chắn đóng cửa sổ an toàn khi đang chạy, theo dõi thay đổi thư mục dự án thời gian thực (**Filesystem Watcher**), bộ lập lịch chạy tự động định kỳ (**Scheduler** + cảnh báo `schedule_missed`), thông báo khi hoàn tất và đăng ký khởi động cùng Windows (`HKCU\...\Run`). |
| **Cài đặt & Bảo mật Khóa** | `src-tauri/src/settings.rs` | Lưu/xóa API key trong **Windows Credential Manager**, ghi nguyên tử `settings.json`, chuẩn hóa `.trim()` URL/model, che khóa ngắn an toàn (`mask_api_key`) và tự động lọc che cả khóa thực tế trong Keyring lẫn `Bearer <token>` / `sk-*` trên mọi dòng `logs` (`message` & `detail`). |
| **Giao diện 5 Màn hình** | `src/App.svelte`, `src/screens/*`, `src/lib/*` | Sidebar thu gọn tự động, nút `STOP` ngay trên thanh tiêu đề, chế độ xem **Bảng ↔ Lưới thẻ Media (Gallery Grid)**, quét & nhập hàng loạt ảnh từ `input/`, trình xem Lightbox phóng to/kéo (`25%–400%`), so sánh trực quan `v1 vs v2` song song & chọn bản chính, bộ lọc ngày/loại + xuất `.csv` & `.log` ở Lịch sử. |
