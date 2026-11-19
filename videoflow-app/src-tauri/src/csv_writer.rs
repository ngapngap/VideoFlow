//! Single-writer queue cho mọi thao tác đọc/ghi prompts.csv (7.2): tuần tự hóa truy cập
//! khi 1–3 dòng chạy song song cùng luồng UI và watcher nền, tránh xung đột `fs::rename` trên NTFS.
//! Dùng `ReentrantMutex` để loại trừ tương hỗ tuyệt đối giữa các luồng khác nhau nhưng không bao giờ
//! tự khóa chết (deadlock) khi cùng một luồng giữ khóa ngoài rồi gọi hàm con trong `prompts_csv`.

use parking_lot::ReentrantMutex;

/// Hàng đợi đồng bộ duy nhất toàn app cho `prompts.csv`.
pub static GLOBAL_QUEUE: ReentrantMutex<()> = ReentrantMutex::new(());
