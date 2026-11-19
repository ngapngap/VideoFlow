//! prompts.csv theo chuẩn Phụ lục v1.3.1 quyết định 3:
//! - Ghi: UTF-8 CÓ BOM, RFC 4180, xuống dòng CRLF
//! - Đọc: chấp nhận có/không BOM; mã khác thì báo lỗi (không tự chuyển mã)
//! - Đồng bộ mọi thao tác đọc/ghi qua `csv_writer::GLOBAL_QUEUE` để không bao giờ đua `fs::rename` trên NTFS
//! - 10 cột A-J (nới lỏng tên cột J theo 4.1, chấp nhận file 9 cột A-I hoặc >10 cột),
//!   khóa dòng = tên ảnh tham chiếu (cột A), chặn khóa rỗng trên dòng có nội dung, chặn path traversal,
//!   trùng tên & trùng file_stem không phân biệt hoa/thường trên Windows NTFS (TC-47).

use crate::error::{AppError, AppResult};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::Path;

pub const HEADER: [&str; 10] = [
    "anh_tham_chieu",  // A
    "promt_tao_image", // B
    "promt_tao_video", // C
    "anh_da_Tao",      // D
    "video_da_tao",    // E
    "ngay_tao_idea",   // F
    "ngay_tao_anh",    // G
    "ngay_tao_video",  // H
    "trang_thai",      // I
    "ghi_chu_loi",     // J (mặc định khi tạo mới; khi đọc chấp nhận tên cột cũ / trống)
];

#[derive(Debug, Clone, Default)]
pub struct Row {
    pub row_no: usize, // số dòng vật lý trong file (1 = header)
    pub anh_tham_chieu: String,
    pub promt_tao_image: String,
    pub promt_tao_video: String,
    pub anh_da_tao: String,
    pub video_da_tao: String,
    pub ngay_tao_idea: String,
    pub ngay_tao_anh: String,
    pub ngay_tao_video: String,
    pub trang_thai: String,
    pub ghi_chu_loi: String,
    /// Các cột phụ sau cột J (nếu người dùng xuất từ bảng có thêm cột K, L...)
    pub extra_columns: Vec<String>,
}

impl Row {
    /// Trạng thái chuẩn hóa theo 4.1: trống = "Đang chờ"
    pub fn status(&self) -> RowStatus {
        match self.trang_thai.trim() {
            "" | "Đang chờ" => RowStatus::Cho,
            "Đã tạo image" => RowStatus::DaTaoImage,
            "Đã tạo video" => RowStatus::DaTaoVideo,
            "Lỗi tạo image" => RowStatus::LoiImage,
            "Lỗi tạo video" => RowStatus::LoiVideo,
            _ => RowStatus::Cho, // giá trị lạ coi như Đang chờ, ghi log ở tầng trên
        }
    }

    /// Dòng EXAMPLE_ không bao giờ chạy (6.7c)
    pub fn is_example(&self) -> bool {
        self.anh_tham_chieu.trim_start().starts_with("EXAMPLE_")
    }

    /// Dòng không có nội dung bị bỏ qua hoàn toàn (5.1 B1)
    pub fn is_empty_content(&self) -> bool {
        self.anh_tham_chieu.trim().is_empty()
            && self.promt_tao_image.trim().is_empty()
            && self.promt_tao_video.trim().is_empty()
    }

    fn to_cells(&self, total_cols: usize) -> Vec<String> {
        let mut cells = vec![
            self.anh_tham_chieu.clone(),
            self.promt_tao_image.clone(),
            self.promt_tao_video.clone(),
            self.anh_da_tao.clone(),
            self.video_da_tao.clone(),
            self.ngay_tao_idea.clone(),
            self.ngay_tao_anh.clone(),
            self.ngay_tao_video.clone(),
            self.trang_thai.clone(),
            self.ghi_chu_loi.clone(),
        ];
        for extra in &self.extra_columns {
            cells.push(extra.clone());
        }
        while cells.len() < total_cols {
            cells.push(String::new());
        }
        cells
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowStatus {
    Cho,
    DaTaoImage,
    DaTaoVideo,
    LoiImage,
    LoiVideo,
}

/// Kiểm tra tên file ảnh tham chiếu hợp lệ:
/// - Không được để trống (`PROMPTS_EMPTY_KEY`) khi dòng có nội dung.
/// - Chỉ tên file trong `input/`, chặn path traversal (`..`, `/`, `\`, `:`).
pub fn validate_row_key(key: &str, row_no: Option<usize>) -> AppResult<()> {
    let trimmed = key.trim();
    let loc = row_no.map(|n| format!(" ở dòng {n}")).unwrap_or_default();
    if trimmed.is_empty() {
        return Err(AppError::new(
            "PROMPTS_EMPTY_KEY",
            format!(
                "Thiếu tên ảnh tham chiếu ở cột A{loc}. Hãy nhập tên file ảnh trong thư mục input/ hoặc xóa dòng nháp trống cột A."
            ),
        ));
    }
    if trimmed.contains("..")
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains(':')
        || trimmed.contains('\0')
    {
        return Err(AppError::new(
            "PROMPTS_INVALID_PATH",
            format!(
                "Tên ảnh tham chiếu không hợp lệ{loc}: '{trimmed}'. Chỉ được nhập tên file nằm trong thư mục input/ (không chứa '..', '/', '\\' hoặc ':')."
            ),
        ));
    }
    Ok(())
}

/// Kiểm tra toàn bộ danh sách dòng: khóa rỗng trên dòng có prompt, path traversal,
/// trùng khóa không phân biệt hoa/thường, và trùng tên gốc (`file_stem`) gây ghi đè sản phẩm đầu ra.
pub fn validate_rows(rows: &[Row]) -> AppResult<()> {
    let mut seen_keys: HashMap<String, (usize, String)> = HashMap::new();
    let mut seen_stems: HashMap<String, (usize, String)> = HashMap::new();
    let mut dup_keys: Vec<String> = Vec::new();
    let mut dup_stems: Vec<String> = Vec::new();

    for r in rows {
        if r.is_empty_content() {
            continue;
        }
        validate_row_key(&r.anh_tham_chieu, Some(r.row_no))?;

        if r.is_example() {
            continue;
        }

        let key_trimmed = r.anh_tham_chieu.trim();
        let lower_key = key_trimmed.to_lowercase();
        let row_label = if r.row_no > 0 { r.row_no } else { 0 };

        if let Some((first_no, first_name)) = seen_keys.get(&lower_key) {
            dup_keys.push(format!(
                "'{}' (dòng {}) trùng với '{}' (dòng {})",
                key_trimmed, row_label, first_name, first_no
            ));
            continue;
        }
        seen_keys.insert(lower_key, (row_label, key_trimmed.to_string()));

        let stem = file_stem_lower(key_trimmed);
        if !stem.is_empty() {
            if let Some((first_no, first_name)) = seen_stems.get(&stem) {
                dup_stems.push(format!(
                    "'{}' (dòng {}) và '{}' (dòng {}) có cùng tên gốc '{}'",
                    first_name, first_no, key_trimmed, row_label, stem
                ));
            } else {
                seen_stems.insert(stem, (row_label, key_trimmed.to_string()));
            }
        }
    }

    if !dup_keys.is_empty() {
        return Err(AppError::new(
            "PROMPTS_DUPLICATE_KEY",
            format!(
                "Tên ảnh tham chiếu bị trùng ở cột A (không phân biệt hoa/thường trên Windows): {}. Copy file ảnh thành tên khác hoặc đổi tên trong cột A rồi nạp lại.",
                dup_keys.join("; ")
            ),
        ));
    }

    if !dup_stems.is_empty() {
        return Err(AppError::new(
            "PROMPTS_DUPLICATE_STEM",
            format!(
                "Tên gốc ảnh tham chiếu bị trùng khiến ảnh/video đầu ra ghi đè lẫn nhau: {}. Hãy đổi tên gốc file ảnh trong thư mục input/ và cột A.",
                dup_stems.join("; ")
            ),
        ));
    }

    Ok(())
}

fn file_stem_lower(file_name: &str) -> String {
    Path::new(file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(file_name)
        .trim()
        .to_lowercase()
}

/// Chuẩn hóa mốc thời gian RFC3339 sang định dạng cột G/H `dd/mm/YYYY HH:MM` của CSV.
pub fn format_csv_timestamp(ts: &str) -> String {
    let trimmed = ts.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(trimmed) {
        return dt
            .with_timezone(&chrono::Local)
            .format("%d/%m/%Y %H:%M")
            .to_string();
    }
    trimmed.to_string()
}

/// Đọc hàng tiêu đề hiện có của file CSV (nếu đọc được) để bảo toàn tên cột J cũ và các cột phụ K..N.
fn read_existing_headers_unlocked(csv_path: &Path) -> Option<Vec<String>> {
    let bytes = fs::read(csv_path).ok()?;
    let content = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    let text = std::str::from_utf8(content).ok()?;
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_reader(text.as_bytes());
    let headers = rdr.headers().ok()?;
    if headers.len() < 9 {
        return None;
    }
    Some(headers.iter().map(|s| s.to_string()).collect())
}

fn read_all_unlocked(csv_path: &Path) -> AppResult<Vec<Row>> {
    let bytes = fs::read(csv_path).map_err(|e| {
        AppError::new(
            "PROMPTS_READ_FAIL",
            format!("Không đọc được prompts.csv: {e}"),
        )
    })?;

    if bytes.is_empty() {
        return Err(AppError::new("PROMPTS_EMPTY", "File prompts.csv rỗng."));
    }

    let content = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);

    let text = String::from_utf8(content.to_vec()).map_err(|_| {
        AppError::new(
            "PROMPTS_ENCODING",
            "File prompts.csv không phải UTF-8 (có thể là ANSI). Hãy mở file bằng Excel và lưu lại ở định dạng 'CSV UTF-8'.",
        )
    })?;

    let mut record_iter = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_reader(text.as_bytes());

    let headers = record_iter.headers().map_err(|e| {
        AppError::new(
            "PROMPTS_BAD_HEADER",
            format!("Không đọc được hàng tiêu đề: {e}"),
        )
    })?;

    let required_a_to_i = &HEADER[..9];
    let mut missing_or_wrong = Vec::new();
    for (idx, expected) in required_a_to_i.iter().enumerate() {
        match headers.get(idx) {
            Some(actual) if actual.trim().eq_ignore_ascii_case(expected) => {}
            _ => missing_or_wrong.push(*expected),
        }
    }
    if !missing_or_wrong.is_empty() {
        return Err(AppError::new(
            "PROMPTS_MISSING_COLUMNS",
            format!(
                "File prompts.csv không đúng cấu trúc cột A–I chuẩn. Thiếu hoặc sai vị trí cột: {}",
                missing_or_wrong.join(", ")
            ),
        ));
    }

    let header_len = headers.len();
    let mut rows = Vec::new();
    for (idx, rec) in record_iter.records().enumerate() {
        let row_no = idx + 2;
        let rec = rec.map_err(|e| {
            AppError::new(
                "PROMPTS_ROW_CORRUPT",
                format!("Dòng {row_no} không đọc được (lỗi định dạng dấu ngoặc kép CSV): {e}"),
            )
        })?;
        let get = |i: usize| rec.get(i).unwrap_or("").trim().to_string();
        let extra_len = rec.len().max(header_len).saturating_sub(10);
        let mut extra_columns = Vec::with_capacity(extra_len);
        for col_idx in 10..(10 + extra_len) {
            extra_columns.push(rec.get(col_idx).unwrap_or("").to_string());
        }

        rows.push(Row {
            row_no,
            anh_tham_chieu: get(0),
            promt_tao_image: get(1),
            promt_tao_video: get(2),
            anh_da_tao: get(3),
            video_da_tao: get(4),
            ngay_tao_idea: get(5),
            ngay_tao_anh: get(6),
            ngay_tao_video: get(7),
            trang_thai: get(8),
            ghi_chu_loi: get(9),
            extra_columns,
        });
    }

    validate_rows(&rows)?;
    Ok(rows)
}

fn write_all_unlocked(csv_path: &Path, rows: &[Row]) -> AppResult<()> {
    validate_rows(rows)?;

    let max_extra = rows.iter().map(|r| r.extra_columns.len()).max().unwrap_or(0);
    let mut header_row: Vec<String> = HEADER.iter().map(|s| (*s).to_string()).collect();
    if let Some(existing) = read_existing_headers_unlocked(csv_path) {
        if let Some(col_j) = existing.get(9) {
            if !col_j.trim().is_empty() {
                header_row[9] = col_j.clone();
            }
        }
        for extra_h in existing.into_iter().skip(10) {
            header_row.push(extra_h);
        }
    }
    while header_row.len() < 10 + max_extra {
        header_row.push(format!("cot_phu_{}", header_row.len() + 1));
    }
    let total_cols = header_row.len();

    let mut out = Vec::with_capacity(4096);
    out.extend_from_slice(&[0xEF, 0xBB, 0xBF]); // UTF-8 BOM

    {
        let mut wtr = csv::WriterBuilder::new()
            .terminator(csv::Terminator::CRLF)
            .quote_style(csv::QuoteStyle::Necessary)
            .from_writer(&mut out);
        let write_result = (|| -> Result<(), csv::Error> {
            wtr.write_record(&header_row)?;
            for r in rows {
                wtr.write_record(r.to_cells(total_cols))?;
            }
            wtr.flush()?;
            Ok(())
        })();
        write_result.map_err(|e| {
            AppError::new(
                "PROMPTS_WRITE_FAIL",
                format!("Không tạo được nội dung prompts.csv: {e}"),
            )
        })?;
    }

    let tmp = csv_path.with_extension("csv.videoflow_tmp");
    {
        let mut f = fs::File::create(&tmp).map_err(|e| {
            AppError::new(
                "PROMPTS_WRITE_FAIL",
                format!("Không tạo được file tạm: {e}"),
            )
        })?;
        if let Err(e) = f.write_all(&out) {
            drop(f);
            let _ = fs::remove_file(&tmp);
            return Err(AppError::new(
                "PROMPTS_WRITE_FAIL",
                format!("Ghi file tạm thất bại: {e}"),
            ));
        }
        let _ = f.sync_all();
    }

    if let Err(e) = fs::rename(&tmp, csv_path) {
        let _ = fs::remove_file(&tmp);
        return Err(AppError::new(
            "CSV_LOCKED",
            format!(
                "Không ghi được prompts.csv (file có thể đang mở trong Excel hoặc ứng dụng khác): {e}"
            ),
        ));
    }
    Ok(())
}

/// Đọc prompts.csv an toàn dưới `GLOBAL_QUEUE` để không bao giờ đua với `fs::rename` trên Windows NTFS.
pub fn read_all(csv_path: &Path) -> AppResult<Vec<Row>> {
    let _guard = crate::csv_writer::GLOBAL_QUEUE.lock();
    read_all_unlocked(csv_path)
}

/// Ghi toàn bộ prompts.csv: UTF-8 BOM + CRLF + RFC 4180 dưới `GLOBAL_QUEUE`.
pub fn write_all(csv_path: &Path, rows: &[Row]) -> AppResult<()> {
    let _guard = crate::csv_writer::GLOBAL_QUEUE.lock();
    write_all_unlocked(csv_path, rows)
}

/// Cập nhật kết quả của MỘT dòng: đọc -> sửa các cột -> ghi lại toàn file nguyên tử dưới `GLOBAL_QUEUE`.
pub fn update_row<F: FnOnce(&mut Row)>(csv_path: &Path, row_key: &str, apply: F) -> AppResult<()> {
    let _guard = crate::csv_writer::GLOBAL_QUEUE.lock();
    let mut rows = read_all_unlocked(csv_path)?;
    let pos = rows
        .iter()
        .position(|r| r.anh_tham_chieu == row_key)
        .or_else(|| {
            rows.iter()
                .position(|r| r.anh_tham_chieu.eq_ignore_ascii_case(row_key))
        })
        .ok_or_else(|| {
            AppError::new(
                "ROW_NOT_FOUND",
                format!("Không tìm thấy dòng có ảnh tham chiếu '{row_key}' trong prompts.csv"),
            )
        })?;
    apply(&mut rows[pos]);
    write_all_unlocked(csv_path, &rows)
}

/// Lưu prompt cột B & C của một dòng (đã `.trim()` đồng nhất với khi đọc CSV).
pub fn save_prompts(
    csv_path: &Path,
    row_key: &str,
    prompt_image: &str,
    prompt_video: &str,
) -> AppResult<()> {
    update_row(csv_path, row_key, |r| {
        r.promt_tao_image = prompt_image.trim().to_string();
        r.promt_tao_video = prompt_video.trim().to_string();
    })
}

/// Đổi khóa ảnh tham chiếu (cột A) của một dòng (kiểm tra khóa rỗng, trùng & path traversal trước khi ghi).
pub fn remap_key(csv_path: &Path, old_key: &str, new_key: &str) -> AppResult<()> {
    validate_row_key(new_key, None)?;
    update_row(csv_path, old_key, |r| {
        r.anh_tham_chieu = new_key.trim().to_string();
    })
}

/// Xóa một dòng khỏi prompts.csv theo khóa ảnh tham chiếu.
pub fn delete_row(csv_path: &Path, row_key: &str) -> AppResult<()> {
    let _guard = crate::csv_writer::GLOBAL_QUEUE.lock();
    let mut rows = read_all_unlocked(csv_path)?;
    let before = rows.len();
    rows.retain(|r| !r.anh_tham_chieu.eq_ignore_ascii_case(row_key));
    if rows.len() == before {
        return Err(AppError::new(
            "ROW_NOT_FOUND",
            format!("Không tìm thấy dòng '{row_key}' trong prompts.csv"),
        ));
    }
    write_all_unlocked(csv_path, &rows)
}

/// Nhập hàng loạt dòng ảnh mới từ `input/` vào cuối `prompts.csv` dưới `GLOBAL_QUEUE`.
/// Mỗi phần tử trong `new_rows` là `(anh_tham_chieu, promt_tao_image, promt_tao_video)`.
/// Bỏ qua các khóa đã tồn tại (không phân biệt hoa/thường), và từ chối nếu trùng `file_stem` khác đuôi.
pub fn append_rows(path: &Path, new_rows: &[(String, String, String)]) -> AppResult<usize> {
    let _guard = crate::csv_writer::GLOBAL_QUEUE.lock();
    if new_rows.is_empty() {
        return Ok(0);
    }

    let mut rows = if path.exists() {
        read_all_unlocked(path)?
    } else {
        Vec::new()
    };

    let mut existing_keys: HashSet<String> = HashSet::new();
    let mut existing_stems: HashMap<String, String> = HashMap::new();
    for r in &rows {
        if r.is_empty_content() || r.is_example() {
            continue;
        }
        let k = r.anh_tham_chieu.trim();
        existing_keys.insert(k.to_lowercase());
        let stem = file_stem_lower(k);
        if !stem.is_empty() {
            existing_stems.insert(stem, k.to_string());
        }
    }

    let now_idea = chrono::Local::now().format("%d/%m/%Y %H:%M").to_string();
    let mut added = 0usize;

    for (raw_key, raw_img_prompt, raw_vid_prompt) in new_rows {
        let key = raw_key.trim();
        validate_row_key(key, None)?;
        if key.starts_with("EXAMPLE_") {
            continue;
        }

        let lower_key = key.to_lowercase();
        if existing_keys.contains(&lower_key) {
            continue;
        }

        let stem = file_stem_lower(key);
        if let Some(conflict_file) = existing_stems.get(&stem) {
            return Err(AppError::new(
                "PROMPTS_DUPLICATE_STEM",
                format!(
                    "Không thể nhập '{key}' vì trùng tên gốc '{stem}' với '{conflict_file}' đã có trong danh sách."
                ),
            ));
        }

        existing_keys.insert(lower_key);
        if !stem.is_empty() {
            existing_stems.insert(stem, key.to_string());
        }

        let next_row_no = rows.len() + 2;
        rows.push(Row {
            row_no: next_row_no,
            anh_tham_chieu: key.to_string(),
            promt_tao_image: raw_img_prompt.trim().to_string(),
            promt_tao_video: raw_vid_prompt.trim().to_string(),
            ngay_tao_idea: now_idea.clone(),
            trang_thai: "Đang chờ".into(),
            ..Default::default()
        });
        added += 1;
    }

    if added > 0 || !path.exists() {
        write_all_unlocked(path, &rows)?;
    }
    Ok(added)
}

/// Đổi phiên bản file sản phẩm đang dùng làm bản chính trên `prompts.csv` dưới `GLOBAL_QUEUE`:
/// - `kind`: `"image"` (hoặc `"anh"`) cập nhật cột D (`anh_da_tao`)
/// - `kind`: `"video"` cập nhật cột E (`video_da_tao`)
pub fn set_active_media_file(
    path: &Path,
    row_key: &str,
    kind: &str,
    filename: &str,
) -> AppResult<()> {
    let trimmed_file = filename.trim();
    validate_row_key(trimmed_file, None)?;
    let norm_kind = kind.trim().to_ascii_lowercase();
    if !matches!(norm_kind.as_str(), "image" | "anh" | "video") {
        return Err(AppError::new(
            "BAD_MEDIA_KIND",
            format!("Loại file sản phẩm không hợp lệ: '{kind}' (chỉ nhận 'image' hoặc 'video')."),
        ));
    }

    update_row(path, row_key, |r| match norm_kind.as_str() {
        "image" | "anh" => r.anh_da_tao = trimmed_file.to_string(),
        "video" => r.video_da_tao = trimmed_file.to_string(),
        _ => {}
    })
}

/// Tạo file mẫu: header + 1 dòng EXAMPLE_ (6.7c)
pub fn create_sample(csv_path: &Path) -> AppResult<()> {
    let _guard = crate::csv_writer::GLOBAL_QUEUE.lock();
    if csv_path.exists() {
        return Err(AppError::new("PROMPTS_EXISTS", "prompts.csv đã tồn tại."));
    }
    let sample = Row {
        row_no: 2,
        anh_tham_chieu: "EXAMPLE_anh_mau.png".into(),
        promt_tao_image: "Dòng ví dụ - xóa dòng này khi nhập dữ liệu thật".into(),
        promt_tao_video: "Dòng ví dụ - xóa dòng này khi nhập dữ liệu thật".into(),
        ..Default::default()
    };
    write_all_unlocked(csv_path, &[sample])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let idx = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "videoflow_csv_test_{}_{}_{}",
            name,
            std::process::id(),
            idx
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp csv dir");
        dir
    }

    #[test]
    fn test_utf8_bom_crlf_roundtrip_and_example_row() {
        let dir = temp_dir("roundtrip");
        let csv_path = dir.join("prompts.csv");

        create_sample(&csv_path).expect("create sample");
        let raw = fs::read(&csv_path).expect("read raw bytes");
        assert!(raw.starts_with(&[0xEF, 0xBB, 0xBF]), "must write UTF-8 BOM");
        let text = std::str::from_utf8(&raw[3..]).expect("valid utf8");
        assert!(text.contains("\r\n"), "must use CRLF line endings");

        let mut rows = read_all(&csv_path).expect("read sample");
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_example());

        rows.push(Row {
            row_no: 3,
            anh_tham_chieu: "sp_01.png".into(),
            promt_tao_image: "Ảnh cô gái cầm ly cà phê, ánh sáng ấm \"studio\"".into(),
            promt_tao_video: "Chuyển động máy quay chậm, mỉm cười".into(),
            trang_thai: String::new(),
            ..Default::default()
        });
        write_all(&csv_path, &rows).expect("write updated rows");

        // Kiểm tra không tự khóa chết khi caller giữ GLOBAL_QUEUE.lock() bên ngoài
        {
            let _outer = crate::csv_writer::GLOBAL_QUEUE.lock();
            save_prompts(
                &csv_path,
                "sp_01.png",
                "  Prompt ảnh mới, có dấu phẩy  ",
                "Prompt video mới\n",
            )
            .expect("save prompts under outer reentrant lock");
        }

        let reread = read_all(&csv_path).expect("reread");
        assert_eq!(reread.len(), 2);
        assert_eq!(reread[1].promt_tao_image, "Prompt ảnh mới, có dấu phẩy");
        assert_eq!(reread[1].promt_tao_video, "Prompt video mới");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_reject_empty_key_on_non_empty_row() {
        let dir = temp_dir("empty_key");
        let csv_p = dir.join("prompts.csv");
        // Dòng 2 có prompt nhưng để trống cột A; dòng 3 trống hoàn toàn (được phép bỏ qua)
        let body = format!(
            "{}\r\n,Prompt nháp chưa điền ảnh,Prompt video,,,,,,,\r\n,,,,,,,,,\r\n",
            HEADER.join(",")
        );
        fs::write(&csv_p, body.as_bytes()).expect("write csv");
        let err = read_all(&csv_p).expect_err("must reject empty col A when prompt exists");
        assert_eq!(err.code, "PROMPTS_EMPTY_KEY");

        // Dòng trống hoàn toàn vẫn được bỏ qua êm ái
        let ok_body = format!("{}\r\nok.png,p1,p2,,,,,,,\r\n,,,,,,,,,\r\n", HEADER.join(","));
        fs::write(&csv_p, ok_body.as_bytes()).expect("write valid csv with blank line");
        let rows = read_all(&csv_p).expect("read valid csv");
        assert_eq!(rows.len(), 2);
        assert!(rows[1].is_empty_content());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_append_rows_and_set_active_media_file() {
        let dir = temp_dir("append_switch");
        let csv_p = dir.join("prompts.csv");
        create_sample(&csv_p).expect("create sample");

        let batch = vec![
            ("bag.png".to_string(), "Ảnh túi da".to_string(), "Video xoay 360".to_string()),
            ("BAG.PNG".to_string(), "Trùng hoa thường".to_string(), "".to_string()),
            ("watch.webp".to_string(), "Đồng hồ".to_string(), "Cận cảnh".to_string()),
        ];
        let added = append_rows(&csv_p, &batch).expect("append batch");
        assert_eq!(added, 2, "duplicate BAG.PNG must be skipped");

        // Thử nhập trùng stem bag.jpg -> báo lỗi PROMPTS_DUPLICATE_STEM
        let stem_err = append_rows(
            &csv_p,
            &[("bag.jpg".to_string(), "p".to_string(), "v".to_string())],
        )
        .expect_err("stem collision must error");
        assert_eq!(stem_err.code, "PROMPTS_DUPLICATE_STEM");

        // Chuyển phiên bản ảnh và video đang hoạt động
        set_active_media_file(&csv_p, "bag.png", "image", "bag_gen_v2.png")
            .expect("switch active image");
        set_active_media_file(&csv_p, "bag.png", "video", "bag_gen_v2.mp4")
            .expect("switch active video");

        let rows = read_all(&csv_p).expect("read after switch");
        let bag = rows.iter().find(|r| r.anh_tham_chieu == "bag.png").unwrap();
        assert_eq!(bag.anh_da_tao, "bag_gen_v2.png");
        assert_eq!(bag.video_da_tao, "bag_gen_v2.mp4");
        assert_eq!(bag.trang_thai, "Đang chờ");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_legacy_col_j_9_cols_and_extra_cols() {
        let dir = temp_dir("legacy_j");

        let csv_9 = dir.join("nine_cols.csv");
        let content_9 = "anh_tham_chieu,promt_tao_image,promt_tao_video,anh_da_Tao,video_da_tao,ngay_tao_idea,ngay_tao_anh,ngay_tao_video,trang_thai\r\nitem1.png,img p,vid p,,,18/11/2026,18/11/2026 09:00,18/11/2026 09:05,Đang chờ\r\n";
        fs::write(&csv_9, content_9.as_bytes()).expect("write 9-col csv");
        let rows_9 = read_all(&csv_9).expect("read 9-col csv");
        assert_eq!(rows_9.len(), 1);
        assert_eq!(rows_9[0].ghi_chu_loi, "");
        assert_eq!(rows_9[0].ngay_tao_anh, "18/11/2026 09:00");
        assert_eq!(rows_9[0].ngay_tao_video, "18/11/2026 09:05");

        let csv_11 = dir.join("eleven_cols.csv");
        let content_11 = "anh_tham_chieu,promt_tao_image,promt_tao_video,anh_da_Tao,video_da_tao,ngay_tao_idea,ngay_tao_anh,ngay_tao_video,trang_thai,ghi_chu_cu,nguoi_phu_trach\r\nitem2.png,p1,p2,,,,,,Đang chờ,ghi chú cũ,Lan\r\n";
        fs::write(&csv_11, content_11.as_bytes()).expect("write 11-col csv");
        let rows_11 = read_all(&csv_11).expect("read 11-col csv");
        assert_eq!(rows_11[0].ghi_chu_loi, "ghi chú cũ");
        assert_eq!(rows_11[0].extra_columns, vec!["Lan".to_string()]);

        update_row(&csv_11, "item2.png", |r| {
            r.trang_thai = "Đã tạo image".into();
            r.ngay_tao_anh = "18/11/2026 12:00".into();
        })
        .expect("update 11-col csv");
        let saved_text = fs::read_to_string(&csv_11).expect("read saved 11-col");
        assert!(saved_text.contains("ghi_chu_cu,nguoi_phu_trach"));
        assert!(saved_text.contains("18/11/2026 12:00"));
        assert!(saved_text.contains("Lan"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_case_insensitive_duplicate_and_stem_collision() {
        let dir = temp_dir("dups");

        let csv_case = dir.join("dup_case.csv");
        let body_case = format!(
            "{}\r\nCat.png,p1,p2,,,,,,,\r\ncat.png,p3,p4,,,,,,,\r\n",
            HEADER.join(",")
        );
        fs::write(&csv_case, body_case.as_bytes()).expect("write dup_case");
        let err_case = read_all(&csv_case).expect_err("must reject Cat.png vs cat.png");
        assert_eq!(err_case.code, "PROMPTS_DUPLICATE_KEY");

        let csv_stem = dir.join("dup_stem.csv");
        let body_stem = format!(
            "{}\r\ncat.png,p1,p2,,,,,,,\r\nCAT.jpg,p3,p4,,,,,,,\r\n",
            HEADER.join(",")
        );
        fs::write(&csv_stem, body_stem.as_bytes()).expect("write dup_stem");
        let err_stem = read_all(&csv_stem).expect_err("must reject cat.png vs CAT.jpg");
        assert_eq!(err_stem.code, "PROMPTS_DUPLICATE_STEM");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_reject_path_traversal() {
        let dir = temp_dir("traversal");
        for bad_name in ["../secret.png", "sub/cat.png", "sub\\cat.png", "C:cat.png"] {
            let csv_p = dir.join("bad.csv");
            let body = format!("{}\r\n{},p1,p2,,,,,,,\r\n", HEADER.join(","), bad_name);
            fs::write(&csv_p, body.as_bytes()).expect("write bad csv");
            let err = read_all(&csv_p).expect_err("must reject path traversal");
            assert_eq!(err.code, "PROMPTS_INVALID_PATH");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_csv_locked_cleans_up_tmp_file() {
        let dir = temp_dir("locked");
        let locked_target = dir.join("prompts.csv");
        fs::create_dir_all(&locked_target).expect("create directory blocking rename");

        let sample = Row {
            row_no: 2,
            anh_tham_chieu: "ok.png".into(),
            promt_tao_image: "p1".into(),
            promt_tao_video: "p2".into(),
            ..Default::default()
        };
        let err =
            write_all(&locked_target, &[sample]).expect_err("rename onto directory must fail");
        assert_eq!(err.code, "CSV_LOCKED");

        let tmp = locked_target.with_extension("csv.videoflow_tmp");
        assert!(
            !tmp.exists(),
            "temporary .videoflow_tmp file must be cleaned up"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
