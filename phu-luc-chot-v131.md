# Phụ lục v1.3.1 — Chốt các quyết định còn lại (VideoFlow Desktop)

> Bản nguồn Markdown của phụ lục, để ghép văn bản vào tài liệu gốc khi regenerate PDF. Bản PDF chính thức: `phu-luc-chot-v131.pdf`.

## Quyết định chốt (tổng quan)

| # | Quyết định | Chốt | Mục bị ảnh hưởng |
|---|-----------|------|------------------|
| 1 | Giãn cách thử lại lỗi tạm thời | **30 giây / 2 phút / 5 phút** (429 tôn trọng Retry-After) | 5.4, 13.5, Thẻ 4 (6.6) |
| 2 | Thời gian poll video | **Giữ 600 giây** + 2 quy tắc làm chắc; đánh giá lại sau 20 lượt thật | 13.3, 5.6 |
| 3 | Mã hóa prompts.csv | **UTF-8 có BOM, RFC 4180** | 4.1, 7.2 |
| 4 | Danh sách dự án gần đây | **Tối đa 5 dự án** + command `list_recent_projects` | 6.7, 13.1 (settings), 13.2 |
| 5 | Vị trí lịch sử | **Lịch sử thuộc state.db từng dự án** — ghi rõ ở 6.5 | 6.5 |

---

## 1. Giãn cách thử lại lỗi tạm thời — quay về nhịp 30 giây / 2 phút / 5 phút

**Quyết định chốt:** mọi lỗi tạm thời (5xx, timeout, lỗi mạng) thử lại tối đa 3 lần mỗi dòng, chờ lùi **30 giây → 2 phút → 5 phút**. Riêng HTTP 429: có header `Retry-After` thì chờ đúng giá trị đó; không có thì dùng cùng bộ ba trên.

**Lý do:** nhịp 3s/10s/30s (tổng ~43 giây) đốt sạch cả 3 lần thử trong đúng cửa sổ sự cố — endpoint video quá tải thường phục hồi theo **phút**, không phải giây; dòng sẽ kẹt trạng thái "Lỗi" và phải đặt lại tay (trái mục tiêu "tự chạy tiếp" ở mục 1.1). Nhịp 30s/2m/5m (~7,5 phút tổng) phủ được hầu hết trục trặc ngắn trong cùng một lượt. Chi phí: một dòng lỗi thật kéo lượt chậm thêm ~7 phút — chấp nhận được vì app chạy nền; đây cũng là nhịp worker cũ đã vận hành thật.

**Văn bản thay thế:**

- **5.4 (tr.10):** thay câu "Lỗi tạm thời (5xx, timeout, lỗi mạng, 429): thử lại tối đa 3 lần mỗi dòng, chờ lùi 3 giây / 10 giây / 30 giây (lỗi 429 tôn trọng Retry-After nếu có)…" bằng:
  > "Lỗi tạm thời (5xx, timeout, lỗi mạng, 429): thử lại tối đa 3 lần mỗi dòng, chờ lùi **30 giây / 2 phút / 5 phút** (lỗi 429 tôn trọng Retry-After nếu có; không có header thì dùng cùng bộ ba trên), tính vào bộ đếm lỗi - bảng đầy đủ ở mục 13.5."
- **13.5 (tr.74), hàng 5xx:** "chờ lùi 3 giây / 10 giây / 30 giây" → "chờ lùi **30 giây / 2 phút / 5 phút**"
- **13.5 (tr.74), hàng 429:** "không có thì chờ lùi 3 giây / 10 giây / 30 giây" → "không có thì chờ lùi **30 giây / 2 phút / 5 phút**"
- **Thẻ 4 (6.6, tr.29):** "giãn cách thử lại: 3 giây / 10 giây / 30 giây…" → "giãn cách thử lại: **30 giây / 2 phút / 5 phút**, lỗi 429 tôn trọng Retry-After (mục 13.5); cấu hình được trong tab này."

Lưu ý: 3s/10s ở TC-09 là **retry khi nạp màn hình** (mục 6.9) — khác bộ đếm này, giữ nguyên không đổi.

## 2. Thời gian poll video — giữ 600 giây, thêm 2 quy tắc làm chắc

**Quyết định chốt:** giữ mặc định poll **600 giây (10 phút)**, cấu hình được. Thêm 2 quy tắc:

1. **Poll timeout không bao giờ tạo task mới.** Lần thử kế tiếp (sau chờ lùi ở 13.5) poll lại đúng task cũ; chỉ tạo task mới khi GET task trả **404 / task not found** (cùng ngoại lệ với mục 13.6). Task đang `running` phía server coi như còn sống.
2. **Làm mới bộ đếm theo progress.** Nếu task có trường `progress` và progress tăng giữa hai lần poll, bộ đếm 600 giây được làm mới — dòng chỉ bị timeout khi progress **đứng yên trọn 10 phút**.

**Đánh giá sau triển khai:** số liệu đo live hiện tại mới có thời gian *tạo task* (5,48 giây), chưa có thời gian tạo video end-to-end. Sau khi chạy thật **20 lượt**, nếu hơn **10%** lượt vượt 600 giây, nâng mặc định lên **900 giây (15 phút)** trong bản cập nhật kế tiếp. Quy tắc (1) đảm bảo không tạo trùng task (không mất quota) trong lúc chờ.

**Văn bản thay thế:**

- **13.3 (tr.72):** thay đoạn "Quá 600 giây (10 phút) chưa xong: dừng poll và coi là timeout…" bằng đoạn gồm 2 quy tắc trên.
- **5.6 (tr.12):** thêm ghi chú đánh giá sau 20 lượt thật vào cuối mục "Cách app xử lý timeout trong luồng".

## 3. prompts.csv — chuẩn UTF-8 có BOM (RFC 4180)

**Quyết định chốt:** file prompts.csv app luôn ghi **UTF-8 có BOM**, phân tách dấu phẩy, bao chuỗi có dấu phẩy/ngoặc kép trong ngoặc kép, xuống dòng **CRLF**. App đọc được cả file có/không BOM; nếu file ở mã khác (ví dụ ANSI), app báo lỗi nạp theo mẫu 6.9 kèm hướng dẫn đổi mã trong Excel — **không tự chuyển mã âm thầm**.

**Lý do:** Excel trên Windows mở file UTF-8 không BOM sẽ vỡ toàn bộ dấu tiếng Việt trong cột prompt — lỗi dữ liệu người dùng khó nhận ra trước khi chạy.

**Vị trí bổ sung:** một đoạn vào cuối mục 4.1 và một câu vào hàng "Đọc" của 7.2.

## 4. Danh sách dự án gần đây — tối đa 5 dự án

**Quyết định chốt:** cấu hình lưu **tối đa 5 dự án mở gần nhất** (khóa `recent_projects` trong bảng settings, giá trị JSON, xếp theo thứ tự mở gần nhất). Nút **Đổi dự án** mở **modal nhỏ 420px** liệt kê 5 dự án (tên thư mục + số dòng); dự án không còn tồn tại trên đĩa hiện **xám + nhãn "không tìm thấy" + nút Xóa khỏi danh sách**; dưới cùng là nút **Chọn thư mục khác**. Chọn một dự án là mở ngay.

**13.2 bổ sung command:**

| Command | Tham số | Trả về |
|---------|---------|--------|
| `list_recent_projects` | `{}` | `{ projects: { path: string, name: string, row_count: number, exists: boolean }[] }` — nguồn cho modal Đổi dự án |

## 5. Lịch sử thuộc về từng dự án — ghi rõ ở 6.5

**Quyết định chốt:** giữ như 13.1 (mọi bảng nằm trong state.db của dự án) và **ghi rõ cho người dùng** ở đầu mục 6.5:

> "Lịch sử và nhật ký nằm trong state.db của từng dự án (mục 13.1): màn hình Lịch sử luôn hiển thị dữ liệu của dự án đang mở; đổi dự án là đổi lịch sử; sao chép cả thư mục dự án sang máy khác mang theo lịch sử của dự án đó. Thống kê 7 ngày ở đầu màn hình cũng tính trên dự án đang mở."

## 6. Ghi chú hợp nhất

Các mục bị sửa/bổ sung: **5.4, 5.6, 4.1, 6.5, 6.6 (Thẻ 4), 6.7, 7.2, 13.2, 13.3, 13.5**. Không mục nào của mục 12 (test case) cần sửa — TC-22 và các TC khác trỏ giãn cách qua "mục 5.4/13.5" nên tự cập nhật theo.

Dòng thêm vào Lịch sử phiên bản (tr.2):

| Phiên bản | Ngày | Nội dung |
|-----------|------|----------|
| v1.3.1 | 17/11/2026 | Phụ lục chốt 5 quyết định còn lại: giãn cách retry quay về 30 giây / 2 phút / 5 phút; quy tắc poll video không tạo task mới + làm mới bộ đếm theo progress; prompts.csv chuẩn UTF-8 có BOM (RFC 4180); danh sách 5 dự án gần đây + command `list_recent_projects`; ghi rõ lịch sử thuộc state.db từng dự án. |
