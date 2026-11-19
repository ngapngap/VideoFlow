# VideoFlow Desktop — Kế hoạch nâng cấp & Sửa lỗi (v1.5.0+)

> Bản cập nhật hoàn chỉnh sau đợt review code ngày 19/11/2026.  
> Cơ sở: Phân tích mã nguồn v1.4.1 + Đối chiếu thực chiến với app WDM Muse Video v1.4.0 (chắt lọc tinh hoa, loại bỏ rườm rà).  
> Quy mô mục tiêu: 1–2 tài khoản, chạy tuần tự trên IP Việt Nam, ưu tiên độ tin cậy và tự động hóa mượt mà.

---

## 0. Quy ước triển khai (Bắt buộc tuân thủ)

- **Cơ sở mã nguồn:** `main` @ `c22f6ea` (sau release v1.4.1). Mỗi mục làm trên một branch riêng (`feat/...` hoặc `fix/...`), rebase sạch sẽ lên `main` trước khi merge.
- **Tiêu chuẩn nghiệm thu (Gate check):**
  - Backend Rust: `cargo test` pass 100%.
  - Sidecar Python: `pytest videoflow-app/sidecar/tests` pass 100% (bổ sung test case cho mọi bugfix/tính năng mới chạm vào pool, quota, date parsing, row_key).
  - Frontend Svelte: `npm run build` và `npx svelte-check` đạt **0 error, 0 warning nghiêm trọng**.
- **Nguyên tắc tương thích ngược:**
  - Không thay đổi tên hay hành vi của các lệnh API hiện hành — chỉ mở rộng hoặc thêm mới.
  - Không phá vỡ cấu trúc file `prompts.csv` (chỉ thêm cột tùy chọn `noi_canh` ở P2-1, nếu file cũ không có cột này thì mặc định là 0).

---

## BẢNG TỔNG HỢP DANH MỤC CÔNG VIỆC

| Mã | Hạng mục | Mức độ | Phạm vi | Mô tả tóm tắt |
|:---|:---|:---:|:---|:---|
| **P0-1** | CDP Reconnect & Hồi phục trình duyệt | 🔥 P0 | Sidecar / Rust | Tự khởi động lại Edge/Chrome khi crash; dọn cả `msedge.exe` lẫn `chrome.exe`; bọc `CDPError` trong Gateway. |
| **P0-2** | Chạy riêng dòng đúng 1 dòng duy nhất | 🔥 P0 | Rust Commands | `run_row` và `rerun_row_step` truyền `key_of_row` thay vì tên ảnh để không chạy cả chùm. |
| **P0-3** | Sửa lỗi 404 Thumbnail ảnh gốc | 🔥 P0 | Frontend Svelte | `Rows.svelte` dùng `image_name` thay vì `row_key` (vốn chứa `#hash`). |
| **P0-4** | Sửa kẹt Quota ngày "Sep 30" (Fail-open) | 🔥 P0 | Sidecar Model | Thêm format date `%b %d` và sửa `quota_exhausted()` thành fail-open khi không parse được ngày. |
| **P0-5** | PATCH cookie xóa đúng `cookie_expires` | 🔥 P0 | Sidecar Admin API | Xóa expiry của cookie khi thay giá trị mới cùng tên (sửa ngược logic `if name in acc.cookies`). |
| **P0-6** | Bỏ khóa cứng sau lỗi (Dòng lỗi luôn chạy lại được) | 🔥 P0 | Rust Engine / CSV | Bỏ guard `MAX_ATTEMPTS_REACHED`; lỗi tạm thời không tính vào trần 3 lần; thêm trạng thái CSV `Lỗi tạm thời`. |
| **P0-7** | Bắt lỗi `UpstreamRefused` thành Lỗi nghiệp vụ | 🔥 P0 | Rust Adapters | Không retry 7,5 phút vô ích khi AI từ chối câu prompt vi phạm chính sách. |
| **P0-8** | Sửa `remap_reference_image` giữ nguyên hash dòng | 🔥 P0 | Rust Commands | Cập nhật `jobs` trong SQLite bằng `actual_key` có hash thay vì tên ảnh trần. |
| **P0-9** | Chuẩn hóa User-Agent sang Windows NT | 🔥 P0 | Sidecar Upstream | Sửa User-Agent Linux trong `muse.py` sang Windows 10/11 tránh bị Cloudflare cắm cờ lệch OS. |
| **P1-1** | Kiểm tra toàn vẹn file tải về | ⚡ P1 | Sidecar / Rust | Ghi `.partial` $\rightarrow$ kiểm tra magic bytes + FFmpeg decode thử trước khi lưu chính thức. |
| **P1-2** | Chờ video mount rồi mới tải (Timeout 6s) | ⚡ P1 | Sidecar Driver | Poll tối đa 6s chờ thẻ `<video>` mount `src` sau khi cuộn trang, tránh tải nhầm video cũ. |
| **P1-3** | Tương quan kết quả bản nhẹ (`set(src)`) | ⚡ P1 | Sidecar Driver | Lưu tập URL media trước khi gửi; chỉ nhận URL mới chưa từng thấy + đúng loại ảnh/video. |
| **P1-4** | Phát hiện khóa tính năng video (`video_locked`) | ⚡ P1 | Sidecar / Tauri | Nhận diện Muse khóa gen video $\rightarrow$ đánh dấu account `video_locked`, dừng batch, bật alert. |
| **P1-5** | Tự mở trình duyệt đăng nhập & thu hoạch token | ⚡ P1 | Sidecar / Frontend | Nút "Đăng nhập": mở browser headed riêng cho account, tự bắt cookie/JWT/headers; nút "Xem trên web". |
| **P1-6** | Chờ upload ảnh tham chiếu xong mới bấm Enter | ⚡ P1 | Sidecar Driver | Chờ `[role=progressbar]` / `[aria-busy=true]` biến mất khỏi input trước khi Send (timeout 45s). |
| **P2-1** | Nối cảnh liên tục bằng frame cuối | 💡 P2 | Rust / FFmpeg | Cột CSV `noi_canh`: dùng FFmpeg cắt frame cuối cảnh trước làm ảnh tham chiếu cảnh sau (kèm fail-safe). |
| **P2-2** | Tự động ghép video sau batch (Auto-Merge) | 💡 P2 | Rust / FFmpeg | Tùy chọn nối các video thành phẩm theo đúng thứ tự dòng CSV (chuẩn hóa FPS/res trước khi concat). |
| **P2-3** | Proxy riêng từng tài khoản | 💡 P2 | Sidecar / Profile | Mỗi account gán 1 proxy riêng, chạy 1 tiến trình Chromium riêng với thư mục profile riêng. |
| **UI-1** | Tab "Tài khoản" riêng trên Sidebar | 🎨 UI | Frontend Svelte | Tách danh sách tài khoản Muse ra khỏi Cài đặt thành tab riêng; có nút Đăng nhập, Xem web, Proxy. |
| **UI-2** | Statusbar IP/Node & Banner cảnh báo nhanh | 🎨 UI | Frontend Svelte | Statusbar hiện node proxy/IP account đang chạy; banner cảnh báo nổi khi account bị khóa video. |
| **UI-3** | Modal xem video lớn trong màn Dòng việc | 🎨 UI | Frontend Svelte | Click đúp thumbnail hoặc icon phóng to để mở modal video lớn kèm prompt, thời lượng, metadata. |

---

## CHI TIẾT KỸ THUẬT TỪNG HẠNG MỤC

### NHÓM P0 — ỔN ĐỊNH LÕI & SỬA LỖI CHÍ MẠNG

#### P0-1 — CDP Reconnect & Tự phục hồi tiến trình trình duyệt
* **Vấn đề v1.4.1:** Driver chỉ kết nối 1 lần lúc khởi động (`startup()`). Nếu Edge/Chrome bị người dùng tắt, bị OS kill hoặc crash, biến `self._browser.closed = True`. Toàn bộ tác vụ sau đó văng lỗi `500 CDPError: cdp connection closed` vĩnh viễn. Ngoài ra `gateway._run` chỉ bắt `except UpstreamError:`, bỏ lọt `CDPError` (vốn kế thừa `RuntimeError`).
* **Vị trí sửa:** 
  * `sidecar/muse2api/drivers/browser/driver.py` (trong `_context`, `_checkout`, `_open_tab`).
  * `sidecar/muse2api/services/gateway.py` (trong `_run`).
  * `sidecar/muse2api/drivers/browser/chromium.py` (`_kill_stale_browsers`).
  * `src-tauri/src/muse_sidecar.rs` (`kill_orphan_edges`).
* **Giải pháp chốt:**
  1. Khi phát hiện `CDPError` hoặc mất kết nối:
     - Gọi `_kill_stale_browsers` dọn dẹp triệt để tiến trình cũ (mở rộng regex tìm cả `msedge.exe` VÀ `chrome.exe`).
     - Xóa các file `SingletonLock`, `SingletonCookie`, `DevToolsActivePort` trong thư mục profile.
     - Chờ cổng CDP (mặc định 19210) giải phóng hoàn toàn (poll 200ms, tối đa 10s).
     - Xóa sạch bộ nhớ tạm `self._tabs.clear()` và `self._contexts.clear()` trong RAM.
     - Gọi lại `startup()` để spawn tiến trình mới và kết nối lại WebSocket.
  2. Bọc `CDPError` trong `gateway._run`: coi đây là lỗi có thể thử lại (`retryable = True`) để kích hoạt failover/retry 1 lần trước khi trả lỗi ra ngoài.
  3. Cập nhật `GET /admin/status` trả thêm cờ `browser_alive: bool` $\rightarrow$ UI Settings hiển thị trạng thái "Trình duyệt đã thoát — Đang tự kết nối lại" thay vì báo "Đang chạy".
* **Nghiệm thu:** Tắt nóng tiến trình Edge qua Task Manager lúc đang chạy $\rightarrow$ request kế tiếp tự khởi động lại Edge thành công, không văng lỗi 500; `pytest` pass.

#### P0-2 — Chạy riêng dòng đúng 1 dòng duy nhất
* **Vấn đề v1.4.1:** Nút "Chạy riêng dòng" hoặc "Chạy lại bước" đang truyền tên file ảnh (`row.anh_tham_chieu`) vào `row_keys`. Nếu 1 ảnh dùng cho 5 dòng CSV, engine sẽ chạy cả 5 dòng cùng lúc.
* **Vị trí sửa:** `src-tauri/src/commands.rs` (dòng ~1228 hàm `run_row` và dòng ~1300 hàm `rerun_row_step`).
* **Giải pháp chốt:** Thay `vec![row.anh_tham_chieu.clone()]` bằng `vec![key_of_row]` (biến khóa chuẩn `anh#hash` đã được tính toán ở dòng ngay phía trên).
* **Nghiệm thu:** Tạo CSV có 3 dòng dùng chung `sample.png` nhưng khác prompt $\rightarrow$ bấm chạy riêng dòng 2, chỉ duy nhất dòng 2 được thực thi; `cargo test` pass.

#### P0-3 — Sửa lỗi 404 Thumbnail ảnh gốc
* **Vấn đề v1.4.1:** `Rows.svelte` lấy `row_key` (có chứa `#hash`) làm tên file nạp ảnh vào `projectFileUrl("input", r.row_key)`. Trình duyệt tìm file `input/photo.png#abc12345` $\rightarrow$ 404 $\rightarrow$ trigger `markThumbBroken` báo sai "Thiếu ảnh gốc".
* **Vị trí sửa:** `videoflow-app/src/screens/Rows.svelte` (dòng 1276 ở dạng bảng, dòng 1504 ở dạng lưới, dòng 1705 và 1836 ở ngăn chi tiết).
* **Giải pháp chốt:** Thay toàn bộ việc đọc tên file ảnh từ `r.row_key` sang `r.image_name` (trường stem đã được backend tách sạch hash).
* **Nghiệm thu:** Thumbnail ảnh gốc hiển thị đúng ảnh thật, không còn hiện cảnh báo thiếu ảnh gốc; `npm run build` pass.

#### P0-4 — Sửa kẹt Quota ngày "Sep 30" (Fail-open)
* **Vấn đề v1.4.1:** Muse hiển thị ngày reset không kèm năm (ví dụ `"Sep 30"`). Hàm `_reset_deadline` không có format phù hợp nên trả về `None`. Tuy nhiên `model.py:66` lại viết: `return reset_in is None or reset_in > 0`, khiến biểu thức trả về `True` $\rightarrow$ tài khoản bị gắn mác hết quota vĩnh viễn (ngược với docstring là không được chặn khi không rõ ngày).
* **Vị trí sửa:** `sidecar/muse2api/accounts/model.py` (hàm `_reset_deadline` và `quota_exhausted`).
* **Giải pháp chốt:**
  1. Thêm format `%b %d` và `%B %d` vào `_RESET_FORMATS`. Khi gặp format này, tự động tính mốc reset là lần xuất hiện sắp tới của ngày đó (nếu ngày đó đã qua trong năm nay thì lấy sang năm sau).
  2. Sửa `quota_exhausted()` thành chuẩn fail-open: `if reset_in is None: return False` (nếu không parse được ngày reset, không được phép chặn tài khoản của người dùng).
* **Nghiệm thu:** Account 100% kèm `resets_at="Sep 30"` không còn bị chặn vĩnh viễn; bổ sung test case trong `test_model.py`; `pytest` pass.

#### P0-5 — PATCH cookie xóa đúng `cookie_expires` cũ
* **Vị trí sửa:** `sidecar/muse2api/api/routes/admin.py` (dòng ~164-167).
* **Vấn đề v1.4.1:** Code đang viết:
  ```python
  acc.cookie_expires = {name: exp for name, exp in (acc.cookie_expires or {}).items() if name in acc.cookies}
  ```
  khiến cookie được thay mới cùng tên vẫn giữ nguyên timestamp hết hạn cũ $\rightarrow$ UI tiếp tục báo "Cookie hết hạn".
* **Giải pháp chốt:** Sửa thành: Xóa bỏ entry trong `acc.cookie_expires` đối với bất kỳ cookie nào có tên xuất hiện trong payload `body.cookies` gửi lên, chỉ giữ lại expiry cho các cookie không bị cập nhật.
* **Nghiệm thu:** Cập nhật cookie mới cho tài khoản hết hạn $\rightarrow$ dòng trạng thái hết hạn lập tức biến mất; `pytest` pass.

#### P0-6 — Bỏ khóa cứng sau lỗi (Dòng lỗi luôn chạy lại được)
* **Vị trí sửa:** `src-tauri/src/commands.rs` (hàm `run_row` ~1209, `reset_row_status_at` ~1385), `engine.rs` (hàm `plan_row` ~621, `record_attempt`).
* **Giải pháp chốt:**
  1. Xóa bỏ hoàn toàn guard chặn `MAX_ATTEMPTS_REACHED` trong `run_row` và `plan_row`. Chỉ số `attempts_count` từ nay chỉ dùng để thống kê/cảnh báo trên giao diện, không được làm khóa cứng chặn người dùng chạy tiếp.
  2. Tạo hàm phân loại `is_transient_error(code, status)`:
     - Lỗi tạm thời (5xx, `CDPError`, timeout mạng, rate limit 429) $\rightarrow$ gọi `record_attempt(..., count_toward_limit = false)`.
     - Lỗi nội dung/nghiệp vụ (prompt vi phạm, cú pháp sai) $\rightarrow$ gọi `count_toward_limit = true`.
  3. Sửa `reset_row_status_at` hỗ trợ tra cứu fallback bằng legacy key để thao tác "Đặt lại trạng thái" luôn có tác dụng 100%.
  4. Trong CSV: bổ sung phân loại `Lỗi tạm thời` (có thể bấm chạy lại ngay) và `Lỗi nội dung` (cần sửa prompt).
* **Nghiệm thu:** Dòng lỗi 3 lần do mạng/CDP vẫn bấm "Chạy riêng dòng" được ngay; `cargo test` pass.

#### P0-7 — Phân loại `UpstreamRefused` thành Lỗi nghiệp vụ
* **Vị trí sửa:** `src-tauri/src/adapters.rs` (trong hàm `classify_http`).
* **Vấn đề v1.4.1:** `UpstreamRefused` trả status 502 nên bị gộp chung vào dải 500..=599 thành `TamThoi`, kích hoạt retry 3 lần mất 7,5 phút.
* **Giải pháp chốt:** Kiểm tra nếu `err_code_json == "upstream_refused"` hoặc body chứa `upstream_refused`, gán ngay `ApiFailKind::NghiepVu` và `code = "PROMPT_REFUSED"`.
* **Nghiệm thu:** Prompt vi phạm chính sách bị Muse từ chối sẽ dừng ngay lập tức, không retry 7,5 phút; `cargo test` pass.

#### P0-8 — Sửa `remap_reference_image` giữ nguyên hash dòng
* **Vị trí sửa:** `src-tauri/src/commands.rs:1543-1561`.
* **Giải pháp chốt:** Khi remap sang ảnh mới `trimmed_new`, tính toán khóa chuẩn:
  `let new_actual_key = prompts_csv::row_key_of(trimmed_new, &row.promt_tao_image, &row.promt_tao_video);`
  và dùng `new_actual_key` để cập nhật bảng `jobs` trong SQLite và gọi `build_row_detail_at(p, &new_actual_key)`.
* **Nghiệm thu:** Lịch sử job và file cũ giữ nguyên sau khi remap ảnh; `cargo test` pass.

#### P0-9 — Chuẩn hóa User-Agent trong Python sang Windows
* **Vị trí sửa:** `sidecar/muse2api/upstream/muse.py:33-36`.
* **Giải pháp chốt:** Đổi chuỗi `USER_AGENT` từ Linux sang Windows 10/11 64-bit đồng nhất với Microsoft Edge:
  `"Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36 Edg/140.0.0.0"`
* **Nghiệm thu:** Gói tin gửi tới `/api/session` có User-Agent Windows đồng nhất với trình duyệt thật; `pytest` pass.

---

### NHÓM P1 — ĐỘ TIN CẬY SINH VIDEO/ẢNH (CHẮT LỌC TỪ WDM)

#### P1-1 — Kiểm tra toàn vẹn file tải về (.partial + Verify)
* **Vị trí sửa:** `sidecar/muse2api/drivers/browser/dom.py` và `driver.py` (hàm `_download`).
* **Giải pháp chốt:**
  1. Tải dữ liệu vào file tạm có đuôi `.partial`.
  2. Kiểm tra magic bytes:
     - PNG: `89 50 4E 47`, JPEG: `FF D8 FF`, WebP: `RIFF...WEBP`, MP4: chứa `ftyp`.
  3. Với video: Gọi lệnh FFmpeg decode thử toàn bộ luồng:
     ```bash
     ffmpeg -v error -i target.partial -f null -
     ```
  4. Nếu đạt chuẩn mới đổi tên `.partial` thành tên file chính thức. Nếu lỗi decode hoặc sai format $\rightarrow$ xóa file partial, tính là 1 lần thử lỗi để kích hoạt retry tự động.
* **Nghiệm thu:** File rác HTML hoặc video cụt đuôi do mạng lag bị chặn đứng ngay, không lưu vào ổ cứng; `pytest` pass.

#### P1-2 — Chờ video mount rồi mới tải (Timeout 6s)
* **Vị trí sửa:** `sidecar/muse2api/drivers/browser/driver.py` (trong hàm `_wait_media`).
* **Giải pháp chốt:** Sau khi cuộn xuống đáy màn hình, thực hiện vòng lặp poll mỗi 100ms (tối đa 6 giây) chờ cho phần tử `<video>` mount thành công thuộc tính `src` (hoặc wrapper xuất hiện `data-hatch-video-src`). Hết 6s nếu chưa thấy thì ghi log warning và tiếp tục phương án fallback, không làm gián đoạn luồng.
* **Nghiệm thu:** Không còn hiện tượng lấy phải thẻ video rỗng do cơ chế ảo hóa DOM của Muse; `pytest` pass.

#### P1-3 — Tương quan kết quả bản nhẹ (`set(src)`)
* **Vị trí sửa:** `sidecar/muse2api/drivers/browser/driver.py` (`_wait_media`).
* **Giải pháp chốt:**
  1. Ngay trước khi bấm gửi (`_send`), lưu lại một `set` chứa toàn bộ `src` của các attachment ảnh/video đang hiện diện trên trang chat.
  2. Khi quét kết quả trả về, loại bỏ toàn bộ các URL đã có trong `set` này. Chỉ chấp nhận các URL mới xuất hiện và có `kind` khớp với tác vụ đang chạy (image/video).
* **Nghiệm thu:** Sinh 2 video liên tiếp không bao giờ bị nhặt nhầm video của lượt trước; `pytest` pass.

#### P1-4 — Phát hiện tài khoản bị khóa tính năng video
* **Vị trí sửa:** `sidecar/muse2api/drivers/browser/driver.py` và `accounts/pool.py`.
* **Giải pháp chốt:**
  1. So khớp câu trả lời của Muse với tập mẫu: *"tính năng tạo video không khả dụng"*, *"video không khả dụng"*, *"không thể tạo video"*, *"video generation is not available"*, *"unable to generate video"*.
  2. Khi khớp mẫu và trên trang không có video: Đánh dấu tài khoản vào trạng thái `video_locked` (tương tự như `COOLING`).
  3. Dừng ngay lượt chạy của tài khoản này, phát cảnh báo nổi lên UI (UI-2), không retry vô ích.
* **Nghiệm thu:** Khi dính khóa video, batch dừng ngay, tài khoản bị highlight đỏ, người dùng nhận thông báo lập tức; `pytest` pass.

#### P1-5 — Tự mở trình duyệt đăng nhập & thu hoạch token
* **Vị trí sửa:** Thêm endpoint `POST /admin/accounts/{id}/open-login` và `POST /admin/accounts/{id}/open-view` trong sidecar; thêm lệnh tương ứng trong Tauri `commands.rs`.
* **Giải pháp chốt:**
  1. **Nút "Đăng nhập":** App tạm ngắt phiên headless của account đó $\rightarrow$ khởi động Chromium có giao diện (headed) với đúng thư mục profile riêng $\rightarrow$ người dùng đăng nhập Muse bằng tay $\rightarrow$ interceptor tự bắt Cookie, Authorization header, localStorage JWT $\rightarrow$ lưu vào `accounts.json` $\rightarrow$ đóng cửa sổ và khởi động lại phiên headless.
  2. **Nút "Xem trên web":** Mở cửa sổ trình duyệt trỏ tới studio URL để xem lại lịch sử/kết quả đã tạo.
  3. Nút Đăng nhập bị vô hiệu hóa khi tài khoản đó đang có tác vụ sinh video dở dang để bảo vệ tiến trình.
* **Nghiệm thu:** Người dùng bấm Đăng nhập $\rightarrow$ đăng nhập xong đóng trình duyệt $\rightarrow$ account tự chuyển sang `active` có token, không cần F12 copy cookie.

#### P1-6 — Chờ upload ảnh tham chiếu xong mới Enter
* **Vị trí sửa:** `sidecar/muse2api/drivers/browser/driver.py` (hàm `_attach` và `_send`).
* **Giải pháp chốt:** Thay thế lệnh `await asyncio.sleep(1.0)` bằng vòng lặp kiểm tra: Chờ cho các thuộc tính `[role=progressbar]` hoặc `[aria-busy=true]` biến mất hoàn toàn khỏi vùng input file (poll 200ms, timeout 45s). Nếu quá 45s thì báo lỗi rõ ràng, tuyệt đối không bấm Enter khi ảnh chưa tải xong.
* **Nghiệm thu:** Đính kèm ảnh 15–20MB trên mạng chập chờn vẫn đảm bảo video sinh ra có ảnh tham chiếu đầy đủ.

---

### NHÓM P2 — TRẢI NGHIỆM & HẬU KỲ TỰ ĐỘNG

#### P2-1 — Nối cảnh liên tục bằng frame cuối (`noi_canh`)
* **Vị trí sửa:** `prompts_csv.rs` (thêm nhận diện cột `noi_canh`) và `engine.rs` (bước B3 tạo video).
* **Giải pháp chốt:**
  1. Thêm cột tùy chọn `noi_canh` (0/1, mặc định 0).
  2. Khi dòng có `noi_canh = 1`: Engine dùng FFmpeg trích xuất khung hình cuối của video dòng hoàn thành liền trước:
     ```bash
     ffmpeg -y -sseof -0.5 -i prev_video.mp4 -frames:v 1 -q:v 2 last_frame.png
     ```
  3. Dùng `last_frame.png` làm ảnh tham chiếu đầu vào cho cảnh này thay thế cho ảnh gốc.
  4. **Fail-safe:** Nếu video cảnh trước bị lỗi hoặc không tìm thấy file, tự động fallback về dùng ảnh tham chiếu gốc của dòng đó và gắn cờ cảnh báo nhẹ, không làm crash luồng.
* **Nghiệm thu:** Bật nối cảnh $\rightarrow$ nhân vật và góc máy của video sau liền mạch với video trước; `cargo test` pass.

#### P2-2 — Tự động ghép video sau batch (Auto-Merge)
* **Vị trí sửa:** `src-tauri/src/settings.rs` và `engine.rs` (cuối hàm chạy batch).
* **Giải pháp chốt:**
  1. Thêm cấu hình `auto_merge: bool` (mặc định tắt) trong Settings.
  2. Khi hoàn tất 100% các dòng của batch: Tự động gom danh sách file video thành phẩm theo đúng thứ tự dòng CSV.
  3. Chuẩn hóa resolution (720x1280) và FPS trước khi nối, bù audio câm (`anullsrc`) cho các clip không tiếng, sau đó chạy concat demuxer tạo file:
     `video_out/<Tên_dự_án>_merged_<YYYYMMDD_HHmm>.mp4`.
  4. Nếu ghép lỗi, giữ nguyên toàn bộ các video thành phần lẻ và thông báo cho người dùng.
* **Nghiệm thu:** Hoàn thành batch $\rightarrow$ xuất hiện ngay 1 video ghép hoàn chỉnh trong thư mục `video_out/`.

#### P2-3 — Proxy riêng từng tài khoản
* **Vị trí sửa:** Sidecar `config.py`, `pool.py`, `driver.py`, `chromium.py` và API `/admin/accounts`.
* **Giải pháp chốt:**
  1. Mỗi account lưu thêm trường `proxy: string | null` (hỗ trợ `socks5://...`, `http://...`).
  2. Nâng cấp `driver.py`: Quản lý map `dict[account_id, BrowserDriver]`. Mỗi account chạy 1 tiến trình Chromium độc lập với cờ `--proxy-server` riêng và thư mục profile riêng tại `%APPDATA%\VideoFlowDesktop\muse\profiles\<account_id>\`.
  3. Account không cấu hình proxy thì đi thẳng (hoặc dùng proxy mặc định nếu có).
* **Nghiệm thu:** Hai tài khoản chạy đồng thời có hai IP đầu ra khác nhau; `pytest` pass.

---

### NHÓM UI — ĐỒNG BỘ GIAO DIỆN

#### UI-1 — Tab "Tài khoản" riêng trên Sidebar
* **Vị trí sửa:** `src/App.svelte` (thêm nav item `"accounts"`, icon `video-source`), tạo mới `src/screens/Accounts.svelte` và `src/lib/AccountCard.svelte`.
* **Bố cục màn hình:**
  - Form thêm account nằm gọn gàng phía trên.
  - Danh sách thẻ tài khoản phía dưới: Hiện rõ Tên, Badge trạng thái, Thanh Quota %, Dòng ngày hết hạn cookie, Nút **Đăng nhập** (mở browser), Nút **Xem trên web**, Ô nhập Proxy riêng (P2-3), Nút Xóa / Bật-tắt.
  - Tab Cài đặt cũ chỉ giữ lại một dòng tóm tắt nhỏ kèm nút *"Mở tab Tài khoản"*.
* **Nghiệm thu:** Mở tab Tài khoản trực tiếp từ Sidebar; các nút đăng nhập/xem web thao tác mượt mà; `npm run build` pass.

#### UI-2 — Statusbar IP/Node & Banner cảnh báo nhanh
* **Vị trí sửa:** `src/App.svelte` (chân trang statusbar và vùng alert đầu trang).
* **Giải pháp chốt:**
  - Statusbar: Thêm một mục hiển thị node proxy đang dùng của account đang chạy (ví dụ: `Node: 127.0.0.1:10808` hoặc `IP: Trực tiếp`).
  - Banner: Khi có account dính trạng thái `video_locked`, hiển thị một thanh thông báo màu vàng/đỏ nổi bật ngay dưới Header: *"Tài khoản X bị khóa tính năng tạo video — [Đổi tài khoản]"*.
* **Nghiệm thu:** Nhìn thấy node proxy ngay ở thanh chân trang; khi dính khóa video có banner nhắc nhở lập tức.

#### UI-3 — Modal xem video lớn trong màn Dòng việc
* **Vị trí sửa:** `src/screens/Rows.svelte` (sử dụng lại `Modal.svelte`).
* **Giải pháp chốt:**
  - Bổ sung sự kiện double-click lên thumbnail video hoặc bấm icon phóng to $\rightarrow$ mở Modal video kích thước 880px.
  - Video phát với controls đầy đủ, kèm khung hiển thị: Prompt ảnh, Prompt video, Thời lượng, Độ phân giải và Tên tài khoản đã sinh.
* **Nghiệm thu:** Double-click video mở modal xem nét căng, bấm Esc hoặc click ngoài để đóng dễ dàng.

---

## THỨ TỰ TRIỂN KHAI ĐỀ XUẤT

$$\text{P0-1, P0-2, P0-3} \rightarrow \text{P0-4, P0-5} \rightarrow \text{P0-6, P0-7, P0-8, P0-9} \rightarrow \text{P1-1, P1-2} \rightarrow \text{P1-6} \rightarrow \text{P1-3, P1-4} \rightarrow \text{P1-5 \& UI-1} \rightarrow \text{P2-3 \& UI-2} \rightarrow \text{P2-1, P2-2 \& UI-3}$$
