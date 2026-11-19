# BÁO CÁO PHÂN TÍCH ĐỐI CHIẾU KỸ THUẬT
## VideoFlow Desktop vs. WDM Muse Video 1.4.0
### Các lỗ hổng tính năng, độ ổn định và lộ trình nâng cấp

---

## 1. TÓM TẮT ĐIỀU HÀNH (EXECUTIVE SUMMARY)

Tài liệu kỹ thuật dịch ngược tĩnh của **WDM Muse Video v1.4.0** cho thấy đối thủ đã giải quyết rất nhiều "bài toán thực chiến" (edge cases) khi tự động hóa nền tảng web `muse.ai` (liên kết với hệ sinh thái Meta AI). 

Trong khi **VideoFlow Desktop (v1.4.1)** sở hữu kiến trúc nền tảng sạch hơn (tách lớp rõ ràng giữa giao diện Svelte, backend Rust/Tauri v2 và Sidecar Python `muse2api` theo chuẩn OpenAI), thì **VideoFlow lại đang thiếu hụt nghiêm trọng các cơ chế phòng vệ runtime, xử lý DOM ảo hóa, hệ thống proxy chống khóa vùng, và các tính năng hậu kỳ thông minh**.

Nếu không bổ sung các cơ chế này, VideoFlow sẽ dễ gặp phải các vấn đề:
1. **Nhặt nhầm video hoặc mất dấu video** khi mạng lag hoặc giao diện chat bị ảo hóa cuộn mất thẻ `<video>`.
2. **Nguy cơ tê liệt hoàn toàn khi Muse kiểm tra vị trí địa lý** vì VideoFlow chưa có proxy định tuyến.
3. **Mất liên kết nhân vật/bối cảnh** giữa các phân cảnh do thiếu cơ chế trích xuất khung hình cuối làm ảnh tham chiếu.
4. **Tải về file rác, file lỗi hoặc video cụt đuôi** do chỉ phụ thuộc vào một phương thức tải base64 duy nhất từ CDP.

---

## 2. SO SÁNH KIẾN TRÚC TỔNG THỂ

| Tiêu chí | VideoFlow Desktop (v1.4.1) | WDM Muse Video (v1.4.0) | Đánh giá & Khác biệt |
|:---|:---|:---|:---|
| **Mô hình kiến trúc** | 3 tầng phân tách: Svelte $\rightarrow$ Rust/Tauri v2 $\rightarrow$ Sidecar Python (`muse2api`) | Monolith: Electron Main $\rightarrow$ Node.js ESM in-process Server $\rightarrow$ Playwright | VideoFlow hiện đại, tốn ít RAM hơn, an toàn bộ nhớ nhờ Rust, chuẩn hóa API OpenAI. |
| **Giao tiếp với Muse** | Chromium CDP qua thư viện `websockets` (Python driver) | Playwright Chromium Persistent Context | WDM tận dụng Playwright Context nên xử lý event mạng và download ổn định hơn. |
| **Phân hệ Proxy** | **Chưa có** (Rust client đang gán cứng `.no_proxy()`) | **Tích hợp nhân Mihomo (Clash core)** chạy ngầm, mixed-port 9090 | VideoFlow hoàn toàn trống mảng này; WDM đã có đầy đủ. |
| **Hậu kỳ video** | Chưa hỗ trợ nối video, trim clip | Hỗ trợ FFmpeg Auto-merge, Muse-merge, Trim, Cut-detection | WDM hoàn thiện trải nghiệm thành phẩm hơn. |

---

## 3. CHI TIẾT CÁC LỖ HỔNG & PHÂN HỆ VIDEOFLOW ĐANG THIẾU

### 3.1. Phân hệ Proxy & Định tuyến vùng (Mihomo Core)
* **Thực trạng VideoFlow:** 
  * Hoàn toàn không có phân hệ proxy.
  * Các client HTTP trong `src-tauri` (`adapters.rs`, `commands.rs`, `muse_sidecar.rs`) đều cấu hình `.no_proxy()`.
  * Khi IP Việt Nam bị Muse.ai chặn hoặc rate-limit, người dùng không có cách nào vượt qua trừ khi dùng VPN toàn máy.
* **Giải pháp của WDM (Module `proxy/engine.js`, `net.js`, `links.js`):**
  * Đóng gói sẵn binary `mihomo-windows-amd64.exe` (Clash core).
  * Hỗ trợ nạp link và subscription: `vmess://`, `vless://` (hỗ trợ Reality/TLS/WS/gRPC), `trojan://`, `ss://`, `http/socks5`.
  * **Geo-routing thông minh:** Chỉ chuyển hướng proxy đối với các domain của Muse (`muse.ai`, `musestudio.ai`, `auth.muse.ai`, `meta.ai`) và CDN media được thêm động (`routeDomain`). Toàn bộ traffic khác đi `DIRECT`.
  * **Cơ chế `ensureNetwork()` & Auto-Fallback:**
    * Giám sát IP định kỳ 30 giây qua `ipwho.is`.
    * Đo độ trễ qua một tiến trình test core riêng biệt (port 19091) không làm gián đoạn tunnel chính.
    * Tự động chuyển node khi IP ra sai quốc gia (mặc định là US).
    * Chặn đứng task không cho gửi nếu mạng chưa đạt chuẩn nhằm bảo vệ tài khoản khỏi bị ban.

---

### 3.2. Cơ chế Tương quan kết quả bằng Marker (Marker Correlation)
* **Thực trạng VideoFlow:**
  * Trong `driver.py` và `dom.py`, VideoFlow ghi nhận kết quả bằng cách so sánh số lượng attachment:
    ```python
    new_atts = st.get("attachments", [])[base_atts:]
    fresh = [a for a in new_atts if a.get("src") and a.get("kind") == kind]
    return fresh[-1]
    ```
  * **Rủi ro lớn:** Nếu trong luồng chat có nhiều hơn một tác vụ sinh ảnh/video hoặc có tin nhắn retry/phản hồi cũ, VideoFlow rất dễ **lấy nhầm video của lần sinh trước** hoặc nhầm ảnh preview tạm.
* **Giải pháp của WDM (Module `muse-generation.js`, `muse-results.js`, `muse-network.js`):**
  * Tự động sinh mã token ngẫu nhiên: `[WDM_REQUEST:<UUID>]` và cài cắm vào prompt chỉ thị: *"Ghi mã [WDM_RESULT:<UUID>] trong phản hồi đi kèm video..."*.
  * **Tương quan 3 lớp bảo vệ:**
    1. **DOM Scoping:** Quét tìm thẻ kết quả nằm chính xác trong lượt trả lời của Assistant có gắn nhãn `WDM_RESULT:<UUID>`, nằm giữa user prompt và user prompt kế tiếp.
    2. **Network Sniffing:** Lắng nghe toàn bộ response JSON của mạng nội bộ trang, duyệt đệ quy (độ sâu 12, 3000 node) để trích xuất URL media có chứa bằng chứng token.
    3. **Grouped Indexing:** Khi Muse gộp nhiều kết quả trong cùng một block phản hồi, phần tử thứ $k$ sẽ tương ứng chính xác với token thứ $k$.
    4. **Strict Correlation:** Với chế độ tạo ảnh hoặc batch song song, chỉ khi nào thấy marker kết quả mới chấp nhận candidate, tuyệt đối không lấy bừa media không rõ nguồn gốc.

---

### 3.3. Xử lý DOM Ảo hóa & Cơ chế Reveal (`revealMuseMedia`)
* **Thực trạng VideoFlow:**
  * VideoFlow chỉ thực hiện cuộn trang xuống đáy bằng script:
    ```javascript
    scrollers.forEach(e => { e.scrollTop = e.scrollHeight; });
    ```
* **Giải pháp của WDM (Module `muse-results.js`):**
  * Muse Studio áp dụng cơ chế ảo hóa DOM (Virtualization): Thẻ `<video>` chỉ được mount vào DOM khi phần tử nằm gần viewport (khoảng 0.3 - 0.8 giây sau khi cuộn tới) và **tự động bị unmount hủy bỏ chỉ sau 0.3 giây khi bị cuộn ra xa**.
  * WDM viết hàm `revealMuseMedia`:
    * Nhận diện chính xác card tin nhắn chứa kết quả.
    * Gọi `scrollIntoView({block: 'center', behavior: 'instant'})`.
    * Poll liên tục mỗi 100ms chờ cho wrapper sinh ra `data-hatch-video-src` hoặc `<video>` có `src`.
    * Quét và tải ngay trong cùng một lock đồng bộ (`exclusive`) trước khi trình duyệt kịp unmount.
    * Nếu request bị trôi, có thuật toán cuộn container theo 4 pha để phục hồi vị trí thẻ.

---

### 3.4. Chiến lược tải file đa tầng (Download Cascade) & Thẩm định toàn vẹn
* **Thực trạng VideoFlow:**
  * Chỉ dựa vào một lệnh duy nhất: `tab.session.evaluate(dom.fetch_as_base64(att["src"]))`.
  * Nếu blob URL bị hết hạn, hoặc bị chặn CORS, hoặc file video bị đứt giữa chừng, VideoFlow sẽ văng lỗi ngay lập tức mà không có cơ chế dự phòng.
* **Giải pháp của WDM (Module `muse-generation.js:downloadResult`):**
  * **Cascade 6 nấc tải file:**
    1. Trích xuất trực tiếp nếu là `data:` URI.
    2. Gọi `fetchBlobToPartial` nếu là `blob:` URL.
    3. Gọi HTTP GET qua context mạng của trình duyệt (chia sẻ cookie/proxy) nếu có URL http(s).
    4. Chờ sự kiện download và click nút `[data-wdm-download]`.
    5. Click vào card kết quả để mở modal preview dialog $\rightarrow$ tìm nút "Download/Export" $\rightarrow$ click tải $\rightarrow$ nhấn `Escape` đóng modal.
    6. Đọc lại source thẻ `<video>` sau khi mount.
  * **Thẩm định tính toàn vẹn (Integrity Verification):**
    * Tải vào file tạm `<target>.partial`.
    * Ảnh: Kiểm tra magic bytes header (PNG: `89 50 4E 47`, JPEG: `FF D8 FF`, WebP: `RIFF...WEBP`).
    * Video: Dùng FFprobe kiểm tra luồng và chạy giải mã thử:
      `ffmpeg -v error -xerror -i partial -map 0:v:0 -map 0:a:0? -f null -`
      loại bỏ hoàn toàn các file tải về bị cụt đuôi, file rác HTML do lỗi 403/502.
    * Chỉ sau khi kiểm tra đạt chuẩn mới rename sang tên file chính thức.
  * **Chống trùng lặp toàn cục (Global SHA-256 Deduplication):**
    * So khớp hash file tải về với toàn bộ các cảnh khác trong dự án; nếu trùng hash thì từ chối vì AI trả về clip cũ.

---

### 3.5. Thu hoạch & Quản lý Credential tự động (Credential Harvesting)
* **Thực trạng VideoFlow:**
  * Người dùng phải cài extension hoặc copy cookie thủ công từ DevTools/EditThisCookie rồi dán vào cài đặt tài khoản.
  * Khi cookie hết hạn, quá trình chạy nền bị tắc, cần can thiệp tay.
* **Giải pháp của WDM (Module `automation/browser.js`, `accounts.js`):**
  * Có nút bấm mở trình duyệt đăng nhập tương tác (`openLoginWindow`) với thư mục profile riêng biệt (`account_<id>`).
  * **Tự động bắt 4 nguồn credential:**
    1. Cookie phiên lọc theo domain `muse.ai` / `meta.ai`.
    2. Header từ các request XHR/fetch (`Authorization`, `x-csrf-token`, v.v.).
    3. Token JWT trong `localStorage` (quét regex `eyJ...`).
    4. Bắt response JSON chứa `access_token` hoặc `csrf_token`.
  * **Tự động làm mới phiên ngầm (`refreshTokenHeadless`):** Định kỳ mở trình duyệt headless trong 5 giây, truy cập studio URL để bắt session token mới sinh ra từ SPA boot XHR.

---

### 3.6. Tính liên tục của kịch bản (Scene Continuation via Last-Frame)
* **Thực trạng VideoFlow:**
  * Mỗi dòng trong `prompts.csv` chạy độc lập. Nếu người dùng muốn cảnh sau tiếp nối cảnh trước, họ phải tự chụp ảnh hoặc chuẩn bị thủ công cột ảnh tham chiếu.
* **Giải pháp của WDM (Module `queue.js:applyContinuation`):**
  * Nhận biết các cảnh thuộc cùng một kịch bản/tập phim (`continuationSource`).
  * Dùng FFmpeg tự động cắt **khung hình cuối cùng (last frame)** của video cảnh trước:
    `ffmpeg -sseof -1 -i prev_video.mp4 -update 1 -q:v 2 last_frame.png`
  * Tự động gán khung hình này làm **ảnh tham chiếu đầu tiên (first reference image)** cho cảnh kế tiếp.
  * Nhờ đó, nhân vật, trang phục và ánh sáng được duy trì liền mạch giữa các cảnh quay liên tiếp.

---

### 3.7. Hậu kỳ & Kiểm soát chất lượng (Auto-Merge & Quality Warning)
* **Thực trạng VideoFlow:**
  * Chỉ dừng lại ở việc sinh ra các file video đơn lẻ vào thư mục `output/`. Người dùng phải tự dùng phần mềm biên tập ngoài để ghép.
* **Giải pháp của WDM (Module `queue.js`, `muse-generation.js`):**
  * **Auto-Merge (FFmpeg):** Sau khi toàn bộ các cảnh trong một batch hoàn thành, tự động dùng FFmpeg concat ghép thành một video hoàn chỉnh.
  * **Muse-Merge (AI Merge):** Gửi một prompt đặc biệt kèm các video thành phần để chính AI Muse xử lý nối chuyển cảnh mượt mà.
  * **Phát hiện cắt cảnh gắt (`detectMidClipCut`):** Dùng FFmpeg phân tích biến động khung hình (`select='gt(scene,0.4)'`). Nếu phát hiện AI tạo ra cú giật/nhảy cảnh bất thường giữa clip, hệ thống đánh dấu cờ `qualityWarning` để người dùng nhận biết và bấm tạo lại.

---

### 3.8. Xử lý khóa tính năng tài khoản (Account Video-Lock Isolation)
* **Thực trạng VideoFlow:**
  * Nếu tài khoản bị Muse khóa tính năng tạo video (nhưng vẫn cho chat/tạo ảnh), hệ thống có thể tiếp tục thử lại nhiều lần theo chính sách retry, gây lãng phí thời gian và kẹt hàng đợi.
* **Giải pháp của WDM (Module `queue.js:handleVideoFeatureUnavailable`):**
  * Phát hiện mẫu phản hồi `VIDEO_FEATURE_UNAVAILABLE` ("tính năng tạo video không khả dụng / bị khóa").
  * Ngay lập tức:
    1. Đánh dấu tài khoản vào trạng thái lỗi chuyên biệt.
    2. Chuyển toàn bộ các submission đang chờ của task sang `refused`.
    3. Hủy bỏ (`stopped`) toàn bộ batch đang chạy của tài khoản đó.
    4. Phát cảnh báo trực quan trên UI để người dùng đổi tài khoản khác.

---

## 4. MA TRẬN ĐỐI CHIẾU TÍNH NĂNG (FEATURE MATRIX)

| Tính năng | VideoFlow Desktop v1.4.1 | WDM Muse Video v1.4.0 | Đánh giá mức độ cần thiết |
|:---|:---:|:---:|:---|
| **Cơ chế Marker UUID tương quan kết quả** | ❌ (chỉ đếm attachment) | ✅ (3 tầng DOM + Network) | 🔴 **Bắt buộc có (P0)** |
| **Cuộn DOM ảo hóa & chờ mount video** | ❌ (chỉ cuộn đáy) | ✅ (`revealMuseMedia`) | 🔴 **Bắt buộc có (P0)** |
| **Cascade 6 nấc tải file & kiểm tra FFmpeg** | ❌ (chỉ tải base64 qua CDP) | ✅ (đầy đủ fallback & verify) | 🔴 **Bắt buộc có (P0)** |
| **Phân hệ Proxy Mihomo & Geo-routing** | ❌ (không có proxy) | ✅ (Mihomo, auto-fallback) | 🟠 **Rất quan trọng (P1)** |
| **Tự trích xuất frame cuối nối cảnh** | ❌ (không hỗ trợ) | ✅ (FFmpeg last frame) | 🟠 **Rất quan trọng (P1)** |
| **Đăng nhập trình duyệt & tự bắt cookie** | ❌ (copy cookie tay) | ✅ (Profile riêng + Interceptor) | 🟡 **Nên có (P2)** |
| **Auto-Merge ghép nối video sau khi xong** | ❌ (không có) | ✅ (FFmpeg & Muse Merge) | 🟡 **Nên có (P2)** |
| **Phát hiện clip bị giật cảnh (Mid-clip cut)** | ❌ (không có) | ✅ (FFmpeg scene detection) | 🟢 **Nâng cao (P3)** |
| **Cách ly tài khoản khi bị khóa quyền video** | ⚠️ (chỉ retry theo mã lỗi) | ✅ (Hủy cả batch, alert rõ) | 🟡 **Nên có (P2)** |

---

## 5. LỘ TRÌNH ĐỀ XUẤT CẢI TIẾN CHO VIDEOFLOW (ROADMAP)

### Giai đoạn 1: Gia cố Độ ổn định lõi (Khẩn cấp - v1.5.0)
1. **Nâng cấp `driver.py` & `dom.py` (Sidecar):**
   * Bổ sung cơ chế sinh mã UUID `[VF_REQUEST:<UUID>]` và `[VF_RESULT:<UUID>]` trong prompt gửi Muse.
   * Viết lại logic trích xuất kết quả: Chỉ nhận diện thẻ media gắn đúng với token UUID của lượt sinh đó.
   * Bổ sung hàm `reveal_element`: Cuộn card vào tâm màn hình và chờ thẻ `<video>` mount trước khi trích xuất `src`.
2. **Nâng cấp cơ chế tải file & kiểm tra toàn vẹn:**
   * Thêm fallback tải: Nếu blob base64 thất bại $\rightarrow$ click nút download của trình duyệt $\rightarrow$ lưu file `.partial`.
   * Chạy kiểm tra FFmpeg trước khi báo `done` cho Tauri.

### Giai đoạn 2: Tích hợp Proxy & Tính năng Nối cảnh (v1.6.0)
1. **Tích hợp Proxy Subsystem vào Tauri:**
   * Tích hợp core Mihomo hoặc cơ chế cấu hình HTTP/SOCKS5 proxy cho cả Rust và Chromium trong sidecar.
   * Thêm tab cấu hình Proxy trong Settings: Cho phép dán link node / subscription, kiểm tra IP và ping.
2. **Tự động nối cảnh (Scene Continuation):**
   * Trong `engine.rs` của Tauri: Nếu dòng CSV được cấu hình `continue_from_previous = true`, gọi FFmpeg cắt khung hình cuối của video dòng trước và đưa vào mảng `reference_images` của dòng hiện tại.

### Giai đoạn 3: Trải nghiệm & Hậu kỳ tự động (v1.7.0)
1. **Cửa sổ đăng nhập trực tiếp (In-app Login Window):**
   * Cho phép mở Chromium có giao diện trực tiếp để người dùng đăng nhập tài khoản Muse, tự động chụp lại cookie/token lưu vào `state.db`.
2. **Auto-Merge Batch:**
   * Thêm tùy chọn "Tự động ghép video sau khi hoàn thành": Sử dụng FFmpeg concat nối các video thành phẩm theo đúng thứ tự dòng CSV.
