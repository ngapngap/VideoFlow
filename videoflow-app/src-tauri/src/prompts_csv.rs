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

/// Kiểm tra xem tên cột có phải là cột nối cảnh hay không (hỗ trợ các biến thể hoa/thường, có/không dấu, gạch dưới/dấu cách).
pub fn is_noi_canh_header(header: impl AsRef<str>) -> bool {
    let s = header.as_ref().trim().to_lowercase();
    matches!(
        s.as_str(),
        "noi_canh"
            | "noi canh"
            | "noicanh"
            | "noi-canh"
            | "nối cảnh"
            | "nối_cảnh"
            | "nối-cảnh"
            | "nốicanh"
    )
}

/// Parse giá trị chuỗi thành boolean nối cảnh (1/0, true/false, yes/no, có/không).
pub fn parse_noi_canh_value(val: &str) -> bool {
    let s = val.trim().to_lowercase();
    matches!(s.as_str(), "1" | "true" | "yes" | "y" | "on" | "có" | "co")
}

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
    /// Cột tùy chọn nối cảnh liên tục từ frame cuối video trước (P2-1)
    pub noi_canh: bool,
    /// Các cột phụ sau cột J (nếu người dùng xuất từ bảng có thêm cột K, L...)
    pub extra_columns: Vec<String>,
}

pub type PromptRow = Row;

impl Row {
    /// Trạng thái chuẩn hóa theo 4.1: trống = "Đang chờ"
    pub fn status(&self) -> RowStatus {
        match self.trang_thai.trim() {
            "" | "Đang chờ" => RowStatus::Cho,
            "Đã tạo image" => RowStatus::DaTaoImage,
            "Đã tạo video" => RowStatus::DaTaoVideo,
            "Lỗi tạo image" => RowStatus::LoiImage,
            "Lỗi tạo video" => RowStatus::LoiVideo,
            "Lỗi tạm thời" => RowStatus::LoiTamThoi,
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
    LoiTamThoi,
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

/// Kiểm tra tên file sản phẩm (cột D `anh_da_tao` hoặc cột E `video_da_tao`):
/// Nếu không trống thì không được chứa ký tự path traversal (`..`, `/`, `\`, `:`, `\0`).
pub fn validate_safe_filename(name: &str, col_label: &str, row_no: Option<usize>) -> AppResult<()> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    let loc = row_no.map(|n| format!(" ở dòng {n}")).unwrap_or_default();
    if trimmed.contains("..")
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains(':')
        || trimmed.contains('\0')
    {
        return Err(AppError::new(
            "PROMPTS_INVALID_PATH",
            format!(
                "Tên file ở {col_label} không hợp lệ{loc}: '{trimmed}'. Chỉ được chứa tên file đơn (không chứa '..', '/', '\\' hoặc ':')."
            ),
        ));
    }
    Ok(())
}

/// Khóa định danh một dòng CSV.
///
/// Trước đây `row_key` **chính là** tên ảnh tham chiếu (cột A), nên thiết kế ngầm giả định
/// 1 ảnh = 1 dòng. Workflow thật lại là **1 ảnh tham chiếu → N dòng với prompt khác nhau**:
/// dùng lại ảnh cho 10 dòng thì bị chặn trùng khóa, và nếu bỏ chặn thì sửa/xóa/trạng thái
/// dính chùm (first-match, hash theo row_key trong state.db, chọn dòng ở UI).
///
/// Khóa mới = `tên ảnh + "#" + 8 ký tự hash(prompt ảnh + prompt video)`:
/// - ổn định khi chèn/xóa/sắp xếp lại dòng trong CSV (không phải số thứ tự),
/// - **không** đổi format file CSV (giá trị khóa suy ra từ dữ liệu, cột A giữ nguyên tên ảnh),
/// - hai dòng chỉ trùng khóa khi **cùng ảnh và cùng prompt** — đó mới đúng là trùng thật.
///
/// Dòng chưa có prompt (nháp) dùng hậu tố `#0` để vẫn phân biệt được với dòng đã điền.
pub fn row_key_of(anh_tham_chieu: &str, prompt_image: &str, prompt_video: &str) -> String {
    let name = anh_tham_chieu.trim();
    let combined = format!("{}\u{1f}{}", prompt_image.trim(), prompt_video.trim());
    if combined == "\u{1f}" {
        // Chưa có prompt nào → chưa thể sinh media, khóa theo tên ảnh là đủ duy nhất
        // (hai dòng cùng ảnh và đều trống prompt là dòng nháp, không có gì để tách).
        return format!("{name}#0");
    }
    let digest = <sha2::Sha256 as sha2::Digest>::digest(combined.as_bytes());
    let hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("{name}#{hex}")
}

/// Tên ảnh (stem) của một khóa dòng — bỏ hậu tố `#hash`. Khóa cũ (chỉ tên ảnh) trả về chính nó.
pub fn row_key_image_name(row_key: &str) -> &str {
    row_key.rsplit_once('#').map_or(row_key, |(name, hash)| {
        if !hash.is_empty() && hash.chars().all(|c| c.is_ascii_hexdigit() || c == '0') {
            name
        } else {
            row_key
        }
    })
}

/// Duyệt mọi dòng của CSV và trả về khóa mới (`row_key_of`) cho từng dòng.
/// Dùng để chuyển `row_key` cũ (tên ảnh) → khóa mới ở mọi nơi tra cứu.
pub fn row_keys_by_legacy_key(rows: &[Row]) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for r in rows {
        let legacy = r.anh_tham_chieu.trim().to_lowercase();
        if legacy.is_empty() {
            continue;
        }
        out.entry(legacy)
            .or_default()
            .push(row_key_of(&r.anh_tham_chieu, &r.promt_tao_image, &r.promt_tao_video));
    }
    out
}

/// Kiểm tra toàn bộ danh sách dòng: khóa rỗng / path traversal trên cột A, và trùng **khóa dòng**
/// (`row_key_of`). Không còn chặn trùng tên ảnh hay trùng `file_stem`: một ảnh dùng cho nhiều
/// dòng là workflow hợp lệ, và việc chống ghi đè file đầu ra đã do `next_version`
/// (`stem_gen.png`, `stem_gen_v2.png`, …) đảm nhiệm.
pub fn validate_rows(rows: &[Row]) -> AppResult<()> {
    let mut seen_keys: HashMap<String, (usize, String)> = HashMap::new();
    let mut dup_keys: Vec<String> = Vec::new();

    for r in rows {
        if r.is_empty_content() {
            continue;
        }
        validate_row_key(&r.anh_tham_chieu, Some(r.row_no))?;
        validate_safe_filename(&r.anh_da_tao, "cột D (anh_da_tao)", Some(r.row_no))?;
        validate_safe_filename(&r.video_da_tao, "cột E (video_da_tao)", Some(r.row_no))?;

        if r.is_example() {
            continue;
        }

        let key = row_key_of(&r.anh_tham_chieu, &r.promt_tao_image, &r.promt_tao_video);
        let lower_key = key.to_lowercase();
        let row_label = if r.row_no > 0 { r.row_no } else { 0 };

        if let Some((first_no, first_key)) = seen_keys.get(&lower_key) {
            dup_keys.push(format!(
                "'{}' (dòng {}) trùng với (dòng {})",
                r.anh_tham_chieu.trim(), row_label, first_no
            ));
            let _ = first_key;
            continue;
        }
        seen_keys.insert(lower_key, (row_label, key));
    }

    if !dup_keys.is_empty() {
        return Err(AppError::new(
            "PROMPTS_DUPLICATE_KEY",
            format!(
                "Có dòng trùng hoàn toàn (cùng ảnh tham chiếu VÀ cùng prompt ảnh + prompt video): {}. \
                 Sửa một trong hai prompt để mỗi dòng tạo ra sản phẩm khác nhau, hoặc xóa dòng thừa.",
                dup_keys.join("; ")
            ),
        ));
    }

    Ok(())
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

struct CsvCacheEntry {
    path: std::path::PathBuf,
    byte_len: usize,
    digest: [u8; 32],
    rows: Vec<Row>,
}

fn csv_cache() -> &'static parking_lot::Mutex<Option<CsvCacheEntry>> {
    static CACHE: std::sync::OnceLock<parking_lot::Mutex<Option<CsvCacheEntry>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| parking_lot::Mutex::new(None))
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

    let digest_arr: [u8; 32] = <sha2::Sha256 as sha2::Digest>::digest(&bytes).into();
    {
        let guard = csv_cache().lock();
        if let Some(entry) = guard.as_ref() {
            if entry.path == csv_path
                && entry.byte_len == bytes.len()
                && entry.digest == digest_arr
            {
                return Ok(entry.rows.clone());
            }
        }
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
    // Chỉ tìm cột `noi_canh` từ cột phụ (index >= 10) để không bao giờ trùng cột J (`ghi_chu_loi`).
    let noi_canh_col_idx: Option<usize> = headers
        .iter()
        .enumerate()
        .skip(10)
        .find(|(_, h)| is_noi_canh_header(h))
        .map(|(i, _)| i);
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

        let noi_canh = if let Some(nc_idx) = noi_canh_col_idx {
            parse_noi_canh_value(rec.get(nc_idx).unwrap_or(""))
        } else {
            false
        };

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
            noi_canh,
            extra_columns,
        });
    }

    validate_rows(&rows)?;
    *csv_cache().lock() = Some(CsvCacheEntry {
        path: csv_path.to_path_buf(),
        byte_len: bytes.len(),
        digest: digest_arr,
        rows: rows.clone(),
    });
    Ok(rows)
}

fn write_all_unlocked(csv_path: &Path, rows: &[Row]) -> AppResult<()> {
    validate_rows(rows)?;

    let max_extra = rows.iter().map(|r| r.extra_columns.len()).max().unwrap_or(0);
    let mut header_row: Vec<String> = HEADER.iter().map(|s| (*s).to_string()).collect();
    let existing_headers = read_existing_headers_unlocked(csv_path);
    let mut nc_col_idx: Option<usize> = None;

    if let Some(existing) = existing_headers {
        if let Some(col_j) = existing.get(9) {
            if !col_j.trim().is_empty() {
                header_row[9] = col_j.clone();
            }
        }
        for (i, extra_h) in existing.into_iter().skip(10).enumerate() {
            if is_noi_canh_header(&extra_h) {
                nc_col_idx = Some(10 + i);
            }
            header_row.push(extra_h);
        }
    }

    let any_noi_canh = rows.iter().any(|r| r.noi_canh);
    if nc_col_idx.is_none() && any_noi_canh {
        nc_col_idx = Some(header_row.len());
        header_row.push("noi_canh".to_string());
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
                let mut cells = r.to_cells(total_cols);
                if let Some(nc_idx) = nc_col_idx {
                    if nc_idx < cells.len() {
                        cells[nc_idx] = if r.noi_canh { "1".to_string() } else { "0".to_string() };
                    }
                }
                wtr.write_record(&cells)?;
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
    let pos = find_row_position(&rows, row_key).ok_or_else(|| {
        AppError::new(
            "ROW_NOT_FOUND",
            format!("Không tìm thấy dòng '{row_key}' trong prompts.csv"),
        )
    })?;
    apply(&mut rows[pos]);
    write_all_unlocked(csv_path, &rows)
}

/// Tìm `Row` theo khóa dòng (dùng chung quy tắc với `update_row`/`delete_row`).
pub fn find_row_by_key<'a>(rows: &'a [Row], row_key: &str) -> Option<&'a Row> {
    find_row_position(rows, row_key).map(|i| &rows[i])
}

/// Định vị dòng theo khóa:
/// 1. khớp `row_key_of` hiện tại (chính xác, kể cả hash prompt),
/// 2. khớp không phân biệt hoa/thường (hash hex có thể khác case qua các bản),
/// 3. fallback: `row_key` là tên ảnh thuần (dữ liệu/lời gọi kiểu cũ) → **chỉ** nhận khi
///    tên ảnh đó xuất hiện đúng một dòng; nhiều dòng cùng ảnh là mơ hồ, KHÔNG đoán bừa.
pub fn find_row_position(rows: &[Row], row_key: &str) -> Option<usize> {
    let target = row_key.trim();
    if target.is_empty() {
        return None;
    }
    let key_of =
        |r: &Row| row_key_of(&r.anh_tham_chieu, &r.promt_tao_image, &r.promt_tao_video);

    if let Some(i) = rows.iter().position(|r| key_of(r) == target) {
        return Some(i);
    }
    if let Some(i) = rows
        .iter()
        .position(|r| key_of(r).eq_ignore_ascii_case(target))
    {
        return Some(i);
    }
    // Legacy: khóa là tên ảnh thuần (không có '#') → chỉ nhận khi duy nhất.
    if !target.contains('#') {
        let matches: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.anh_tham_chieu.eq_ignore_ascii_case(target))
            .map(|(i, _)| i)
            .collect();
        if matches.len() == 1 {
            return Some(matches[0]);
        }
    }
    None
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

/// Xóa **đúng một** dòng khỏi prompts.csv theo khóa dòng.
///
/// Trước đây hàm retain theo tên ảnh nên xóa một dòng là xóa luôn mọi dòng dùng chung ảnh.
pub fn delete_row(csv_path: &Path, row_key: &str) -> AppResult<()> {
    let _guard = crate::csv_writer::GLOBAL_QUEUE.lock();
    let mut rows = read_all_unlocked(csv_path)?;
    let pos = find_row_position(&rows, row_key).ok_or_else(|| {
        AppError::new(
            "ROW_NOT_FOUND",
            format!("Không tìm thấy dòng '{row_key}' trong prompts.csv"),
        )
    })?;
    rows.remove(pos);
    write_all_unlocked(csv_path, &rows)
}

/// Nhập hàng loạt dòng ảnh mới từ `input/` vào cuối `prompts.csv` dưới `GLOBAL_QUEUE`.
/// Mỗi phần tử trong `new_rows` là `(anh_tham_chieu, promt_tao_image, promt_tao_video)`.
/// Bỏ qua khóa dòng đã tồn tại (`row_key_of`: cùng ảnh **và** cùng prompt).
/// Không còn chặn trùng tên ảnh/`file_stem`: một ảnh dùng cho nhiều dòng là hợp lệ,
/// chống ghi đè đầu ra do `next_version` lo.
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
    for r in &rows {
        if r.is_empty_content() || r.is_example() {
            continue;
        }
        existing_keys.insert(
            row_key_of(&r.anh_tham_chieu, &r.promt_tao_image, &r.promt_tao_video)
                .to_lowercase(),
        );
    }

    let now_idea = chrono::Local::now().format("%d/%m/%Y %H:%M").to_string();
    let mut added = 0usize;

    for (raw_key, raw_img_prompt, raw_vid_prompt) in new_rows {
        let key = raw_key.trim();
        validate_row_key(key, None)?;
        if key.starts_with("EXAMPLE_") {
            continue;
        }

        let img_prompt = raw_img_prompt.trim();
        let vid_prompt = raw_vid_prompt.trim();
        let row_key = row_key_of(key, img_prompt, vid_prompt).to_lowercase();
        if existing_keys.contains(&row_key) {
            continue;
        }
        existing_keys.insert(row_key);

        let next_row_no = rows.len() + 2;
        rows.push(Row {
            row_no: next_row_no,
            anh_tham_chieu: key.to_string(),
            promt_tao_image: img_prompt.to_string(),
            promt_tao_video: vid_prompt.to_string(),
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
        // Cả 3 dòng khác prompt nên không dòng nào là trùng thật — kể cả BAG.PNG (cùng
        // ảnh, khác prompt) vẫn là dòng hợp lệ.
        assert_eq!(added, 3, "mỗi dòng khác prompt là dòng mới hợp lệ");

        // Cùng ảnh nhưng prompt khác → là dòng MỚI hợp lệ (workflow 1 ảnh → N video).
        let added_same_image = append_rows(
            &csv_p,
            &[(
                "bag.png".to_string(),
                "Ảnh túi da mặt trước".to_string(),
                "Video cận cảnh khoá".to_string(),
            )],
        )
        .expect("same image with a different prompt must be accepted");
        assert_eq!(added_same_image, 1);

        // Chuyển phiên bản ảnh/video đang hoạt động — dùng KHOÁ DÒNG (bag.png có 2 dòng
        // cùng ảnh nên tên ảnh thuần là mơ hồ, đúng như thiết kế mới).
        let bag_key = row_key_of("bag.png", "Ảnh túi da", "Video xoay 360");
        set_active_media_file(&csv_p, &bag_key, "image", "bag_gen_v2.png")
            .expect("switch active image");
        set_active_media_file(&csv_p, &bag_key, "video", "bag_gen_v2.mp4")
            .expect("switch active video");

        let rows = read_all(&csv_p).expect("read after switch");
        let bag = rows
            .iter()
            .find(|r| {
                row_key_of(&r.anh_tham_chieu, &r.promt_tao_image, &r.promt_tao_video) == bag_key
            })
            .expect("bag row");
        assert_eq!(bag.anh_da_tao, "bag_gen_v2.png");
        assert_eq!(bag.video_da_tao, "bag_gen_v2.mp4");

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
    fn test_cung_anh_khac_prompt_la_hai_dong_hop_le() {
        let dir = temp_dir("same_image");
        let csv = dir.join("same_image.csv");
        // 1 ảnh tham chiếu → 2 dòng prompt khác nhau: hợp lệ (workflow thật).
        let body = format!(
            "{}\r\ncat.png,ảnh mèo ngồi,video xoay,,,,,,,\r\ncat.png,ảnh mèo đứng,video zoom,,,,,,,\r\n",
            HEADER.join(",")
        );
        fs::write(&csv, body.as_bytes()).expect("write same_image");
        let rows = read_all(&csv).expect("same image + different prompts must be accepted");
        assert_eq!(rows.len(), 2);

        let k1 = row_key_of(&rows[0].anh_tham_chieu, &rows[0].promt_tao_image, &rows[0].promt_tao_video);
        let k2 = row_key_of(&rows[1].anh_tham_chieu, &rows[1].promt_tao_image, &rows[1].promt_tao_video);
        assert_ne!(k1, k2, "cùng ảnh nhưng khác prompt phải khác khoá");
        assert!(k1.starts_with("cat.png#"), "khoá phải giữ tên ảnh: {k1}");
        assert_eq!(row_key_image_name(&k1), "cat.png");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_trung_hoan_toan_moi_bao_loi() {
        let dir = temp_dir("dup_exact");
        let csv = dir.join("dup_exact.csv");
        // Cùng ảnh + cùng prompt → trùng thật.
        let body = format!(
            "{}\r\ncat.png,p1,p2,,,,,,,\r\ncat.png,p1,p2,,,,,,,\r\n",
            HEADER.join(",")
        );
        fs::write(&csv, body.as_bytes()).expect("write dup_exact");
        let err = read_all(&csv).expect_err("identical rows must be rejected");
        assert_eq!(err.code, "PROMPTS_DUPLICATE_KEY");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_xoa_dung_mot_dong_khi_nhieu_dong_cung_anh() {
        let dir = temp_dir("delete_one");
        let csv = dir.join("one.csv");
        let body = format!(
            "{}\r\ncat.png,pA,vA,,,,,,,\r\ncat.png,pB,vB,,,,,,,\r\n",
            HEADER.join(",")
        );
        fs::write(&csv, body.as_bytes()).expect("write one");
        let rows = read_all(&csv).expect("read");
        let key_b = row_key_of(&rows[1].anh_tham_chieu, &rows[1].promt_tao_image, &rows[1].promt_tao_video);

        delete_row(&csv, &key_b).expect("delete row B");
        let after = read_all(&csv).expect("read after delete");
        assert_eq!(after.len(), 1, "chỉ xoá đúng 1 dòng, không xoá cả chùm cùng ảnh");
        assert_eq!(after[0].promt_tao_image, "pA");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_sua_dung_dong_theo_khoa_moi() {
        let dir = temp_dir("update_one");
        let csv = dir.join("upd.csv");
        let body = format!(
            "{}\r\ncat.png,pA,vA,,,,,,,\r\ncat.png,pB,vB,,,,,,,\r\n",
            HEADER.join(",")
        );
        fs::write(&csv, body.as_bytes()).expect("write upd");
        let rows = read_all(&csv).expect("read");
        let key_b = row_key_of(&rows[1].anh_tham_chieu, &rows[1].promt_tao_image, &rows[1].promt_tao_video);

        update_row(&csv, &key_b, |r| r.ghi_chu_loi = "ghi-chu-B".into()).expect("update B");
        let after = read_all(&csv).expect("read after update");
        assert_eq!(after[0].ghi_chu_loi, "", "dòng A không được đổi theo");
        assert_eq!(after[1].ghi_chu_loi, "ghi-chu-B");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_khoa_cu_chi_nhan_khi_duy_nhat() {
        let dir = temp_dir("legacy_key");
        let csv = dir.join("legacy.csv");
        let body = format!(
            "{}\r\ncat.png,pA,vA,,,,,,,\r\ncat.png,pB,vB,,,,,,,\r\n",
            HEADER.join(",")
        );
        fs::write(&csv, body.as_bytes()).expect("write legacy");
        // Khoá legacy (chỉ tên ảnh) với 2 dòng cùng ảnh là mơ hồ → phải báo không tìm thấy,
        // KHÔNG được đoán bừa dòng đầu tiên.
        let err = update_row(&csv, "cat.png", |r| r.ghi_chu_loi = "x".into())
            .expect_err("legacy key must be ambiguous");
        assert_eq!(err.code, "ROW_NOT_FOUND");

        // Với 1 dòng duy nhất thì khoá legacy vẫn dùng được (tương thích dữ liệu cũ).
        let csv_single = dir.join("single.csv");
        let body_single = format!("{}\r\ncat.png,pA,vA,,,,,,,\r\n", HEADER.join(","));
        fs::write(&csv_single, body_single.as_bytes()).expect("write single");
        update_row(&csv_single, "cat.png", |r| r.ghi_chu_loi = "da-sua".into())
            .expect("legacy key works when unique");
        let after = read_all(&csv_single).expect("read");
        assert_eq!(after[0].ghi_chu_loi, "da-sua");
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

    #[test]
    fn test_noi_canh_header_variants_and_value_parsing() {
        assert!(is_noi_canh_header("noi_canh"));
        assert!(is_noi_canh_header("NOI_CANH"));
        assert!(is_noi_canh_header("noi canh"));
        assert!(is_noi_canh_header("nối cảnh"));
        assert!(is_noi_canh_header("Nối Cảnh"));
        assert!(is_noi_canh_header("nối_cảnh"));
        assert!(is_noi_canh_header("noicanh"));
        assert!(is_noi_canh_header("noi-canh"));
        assert!(!is_noi_canh_header("ghi_chu_loi"));
        assert!(!is_noi_canh_header("other_col"));

        // True values
        assert!(parse_noi_canh_value("1"));
        assert!(parse_noi_canh_value("true"));
        assert!(parse_noi_canh_value("True"));
        assert!(parse_noi_canh_value("TRUE"));
        assert!(parse_noi_canh_value("yes"));
        assert!(parse_noi_canh_value("YES"));
        assert!(parse_noi_canh_value("y"));
        assert!(parse_noi_canh_value("có"));
        assert!(parse_noi_canh_value("CÓ"));
        assert!(parse_noi_canh_value("co"));
        assert!(parse_noi_canh_value("on"));

        // False values
        assert!(!parse_noi_canh_value("0"));
        assert!(!parse_noi_canh_value("false"));
        assert!(!parse_noi_canh_value("False"));
        assert!(!parse_noi_canh_value("no"));
        assert!(!parse_noi_canh_value("không"));
        assert!(!parse_noi_canh_value("khong"));
        assert!(!parse_noi_canh_value(""));
        assert!(!parse_noi_canh_value("   "));
        assert!(!parse_noi_canh_value("invalid_val"));
    }

    #[test]
    fn test_parse_csv_without_noi_canh_column_defaults_to_false() {
        let dir = temp_dir("no_nc");
        let csv_p = dir.join("prompts.csv");
        let body = format!(
            "{}\r\ncat.png,p1,v1,,,,,,,\r\ndog.png,p2,v2,,,,,,,\r\n",
            HEADER.join(",")
        );
        fs::write(&csv_p, body.as_bytes()).expect("write csv");
        let rows = read_all(&csv_p).expect("read csv");
        assert_eq!(rows.len(), 2);
        assert!(!rows[0].noi_canh);
        assert!(!rows[1].noi_canh);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_parse_csv_with_noi_canh_column_and_roundtrip() {
        let dir = temp_dir("with_nc");
        let csv_p = dir.join("prompts.csv");
        // Test with Vietnamese header "nối cảnh"
        let header = format!("{},nối cảnh", HEADER.join(","));
        let body = format!(
            "{header}\r\nscene1.png,p1,v1,,,,,,,,0\r\nscene2.png,p2,v2,,,,,,,,1\r\nscene3.png,p3,v3,,,,,,,,true\r\nscene4.png,p4,v4,,,,,,,,có\r\nscene5.png,p5,v5,,,,,,,,no\r\n"
        );
        fs::write(&csv_p, body.as_bytes()).expect("write csv");

        let rows = read_all(&csv_p).expect("read csv");
        assert_eq!(rows.len(), 5);
        assert!(!rows[0].noi_canh, "scene1 must be false (0)");
        assert!(rows[1].noi_canh, "scene2 must be true (1)");
        assert!(rows[2].noi_canh, "scene3 must be true (true)");
        assert!(rows[3].noi_canh, "scene4 must be true (có)");
        assert!(!rows[4].noi_canh, "scene5 must be false (no)");

        // Modify a row and write back
        let mut modified = rows.clone();
        modified[0].noi_canh = true;
        write_all(&csv_p, &modified).expect("write modified");

        let reread = read_all(&csv_p).expect("reread");
        assert_eq!(reread.len(), 5);
        assert!(reread[0].noi_canh, "scene1 now true after update");
        assert!(reread[1].noi_canh, "scene2 still true");
        assert!(reread[2].noi_canh, "scene3 still true");
        assert!(reread[3].noi_canh, "scene4 still true");
        assert!(!reread[4].noi_canh, "scene5 still false");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_reject_path_traversal_on_generated_columns_d_and_e() {
        let bad_img = Row {
            row_no: 2,
            anh_tham_chieu: "cat.png".into(),
            promt_tao_image: "p1".into(),
            promt_tao_video: "v1".into(),
            anh_da_tao: "../secret.png".into(),
            ..Default::default()
        };
        let err_d = validate_rows(&[bad_img]).unwrap_err();
        assert_eq!(err_d.code, "PROMPTS_INVALID_PATH");

        let bad_vid = Row {
            row_no: 3,
            anh_tham_chieu: "cat.png".into(),
            promt_tao_image: "p1".into(),
            promt_tao_video: "v1".into(),
            video_da_tao: r"C:\Windows\system.ini".into(),
            ..Default::default()
        };
        let err_e = validate_rows(&[bad_vid]).unwrap_err();
        assert_eq!(err_e.code, "PROMPTS_INVALID_PATH");
    }
}
