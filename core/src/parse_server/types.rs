//! Shared types for the parse-only server: task records, engine routing,
//! content hashing, key masking and the hand-written multipart parser.
//!
//! Engine routing and the MD5-dedup layout mirror mark-files-down
//! (`api/file_handler.py`, `api/task_store.py`): the task id *is* the
//! content MD5, uploads live at `md5[:2]/md5[2:4]/md5+ext`.
use md5::Digest as _;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    #[default]
    Pending,
    Processing,
    Completed,
    Failed,
}
impl JobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Processing => "processing",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
    pub fn from_str(value: &str) -> Option<Self> {
        Some(match value {
            "pending" => Self::Pending,
            "processing" => Self::Processing,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            _ => return None,
        })
    }
}

/// One engine attempt, persisted as JSON on the task row. `Attempt` from
/// `crate::conversion` is converted into this at the callback boundary so
/// the store never depends on conversion internals.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AttemptRecord {
    pub engine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segment_current: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segment_total: Option<u32>,
    pub elapsed_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ParseJobRecord {
    pub task_id: String,
    pub input_md5: String,
    /// Path relative to the uploads root, e.g. `ab/cd/<md5>.pdf`.
    pub input_file: String,
    /// Path relative to the outputs root, e.g. `temp/ab/cd/<md5>.md`.
    pub output_file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    pub engine: String,
    pub status: JobStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default)]
    pub attempts: Vec<AttemptRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pages: Option<u32>,
    /// Live progress snapshot: current engine / phase / segment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<String>,
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn md5_hex(bytes: &[u8]) -> String {
    format!("{:x}", md5::Md5::digest(bytes))
}

pub fn md5_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hash = md5::Md5::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

/// Mask a secret for logs/API: only the last 4 characters survive.
pub fn mask_key(key: &str) -> String {
    let key = key.trim();
    if key.len() <= 4 {
        "****".into()
    } else {
        format!("****{}", &key[key.len() - 4..])
    }
}

/// Layered upload path for a content MD5, mirroring mark-files-down:
/// `md5[:2]/md5[2:4]/md5+ext`.
pub fn upload_relative_path(md5: &str, ext: &str) -> PathBuf {
    PathBuf::from(&md5[..2])
        .join(&md5[2..4])
        .join(format!("{md5}{ext}"))
}

/// Default output path, mirroring mark-files-down `task_store.submit`:
/// `temp/<md5[:2]>/<md5[2:4]>/<task_id>.md`.
pub fn default_output_relative_path(task_id: &str) -> PathBuf {
    PathBuf::from("temp")
        .join(&task_id[..2])
        .join(&task_id[2..4])
        .join(format!("{task_id}.md"))
}

/// Reject absolute paths and `..` escapes. Used for every user-supplied
/// relative path (input_file / output_file) before it touches the disk.
pub fn safe_relative_path(value: &str) -> Option<PathBuf> {
    let path = Path::new(value);
    if path.is_absolute() {
        return None;
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    if out.as_os_str().is_empty() {
        return None;
    }
    Some(out)
}

/// Engine routing, mirroring mark-files-down `get_engine_for_extension`
/// (MinerU first, then LibreOffice/anydoc, video before audio, markitdown
/// last). SAG has no LibreOffice/MarkItDown engines: office and text
/// formats route to `anydoc` (built-in converter, Docling fallback inside
/// the conversion chain) and keep their label for the task record.
pub fn engine_for_extension(ext: &str) -> Option<&'static str> {
    let ext = ext.trim_start_matches('.').to_ascii_lowercase();
    match ext.as_str() {
        "pdf" | "jpg" | "jpeg" | "png" | "bmp" | "tiff" | "tif" | "webp" | "gif" | "svg" => {
            Some("mineru")
        }
        "doc" | "docx" | "ppt" | "pptx" | "xls" | "xlsx" | "rtf" | "wps" | "odt" | "ods"
        | "odp" | "html" | "htm" | "csv" | "json" | "xml" | "txt" | "md" | "zip" | "epub"
        | "msg" => Some("anydoc"),
        "mp4" | "mov" | "avi" | "mkv" => Some("video"),
        "wav" | "mp3" | "flac" | "m4a" | "ogg" | "opus" | "webm" => Some("asr"),
        _ => None,
    }
}

/// Recompute the routing engine from the task's input file, mirroring
/// mark-files-down `resolve_task_engine`: the persisted engine reflects the
/// last actual path (e.g. a MinerU failure that fell back), while a restart
/// must route from the file type again.
pub fn resolve_task_engine(input_file: &str, fallback: &str) -> String {
    let ext = Path::new(input_file)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    engine_for_extension(ext)
        .map(str::to_owned)
        .unwrap_or_else(|| fallback.to_owned())
}

/// Plain-text formats that are already Markdown-compatible. The desktop
/// indexer reads these directly (`core/src/text.rs` `file_kind`) and never
/// runs them through the conversion chain: anydoc has no txt/md/json/xml,
/// MinerU does not take them, and Docling is the only other candidate — so
/// without the Docling component installed they would fail with
/// "needs Docling component". The parse server reads them straight
/// through instead (job.rs `run_document` fast path, engine `"text"`).
/// HTML is deliberately excluded: returning raw HTML source is not a
/// parse, it still needs a converter (Docling).
pub fn is_plain_text_extension(ext: &str) -> bool {
    matches!(
        ext.trim_start_matches('.').to_ascii_lowercase().as_str(),
        "txt" | "md" | "markdown" | "rst" | "json" | "xml"
    )
}

pub fn is_audio_extension(ext: &str) -> bool {
    matches!(
        ext.trim_start_matches('.').to_ascii_lowercase().as_str(),
        "wav" | "mp3" | "flac" | "m4a" | "ogg" | "opus" | "webm"
    )
}
pub fn is_video_extension(ext: &str) -> bool {
    matches!(
        ext.trim_start_matches('.').to_ascii_lowercase().as_str(),
        "mp4" | "mov" | "avi" | "mkv"
    )
}

/// Document groups for the per-file-type engine preference
/// (`settings.parse.{pdf,office,image}_engine`): PDF, Office-style
/// documents, and images. Plain text / Markdown have no engine choice:
/// they are read straight through (see [`is_plain_text_extension`])
/// instead of entering the conversion chain, where no engine owns them.
pub fn doc_engine_group(ext: &str) -> Option<&'static str> {
    match ext.trim_start_matches('.').to_ascii_lowercase().as_str() {
        "pdf" => Some("pdf"),
        "doc" | "docx" | "ppt" | "pptx" | "xls" | "xlsx" | "rtf" | "wps" | "odt" | "ods"
        | "odp" | "epub" => Some("office"),
        "jpg" | "jpeg" | "png" | "bmp" | "tiff" | "tif" | "webp" | "gif" | "svg" => {
            Some("image")
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Minimal multipart/form-data parser (hyper + hand-rolled; no new crates).
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct MultipartPart {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub data: Vec<u8>,
}
impl MultipartPart {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.data).trim().to_string()
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// Parse `multipart/form-data` body. Returns an error string on malformed input.
pub fn parse_multipart(body: &[u8], boundary: &str) -> Result<Vec<MultipartPart>, String> {
    let delimiter = format!("--{boundary}").into_bytes();
    let mut parts = Vec::new();
    let mut cursor =
        find_subslice(body, &delimiter, 0).ok_or_else(|| "multipart 缺少起始边界".to_string())?;
    loop {
        cursor += delimiter.len();
        // `--boundary--` ends the body.
        if body.get(cursor..cursor + 2) == Some(b"--".as_slice()) {
            break;
        }
        // Skip CRLF after the boundary line.
        if body.get(cursor..cursor + 2) == Some(b"\r\n".as_slice()) {
            cursor += 2;
        } else if body.get(cursor..cursor + 1) == Some(b"\n".as_slice()) {
            cursor += 1;
        }
        let header_end = find_subslice(body, b"\r\n\r\n", cursor)
            .ok_or_else(|| "multipart part 缺少头部结束".to_string())?;
        let headers = String::from_utf8_lossy(&body[cursor..header_end]).into_owned();
        let data_start = header_end + 4;
        // Next boundary is preceded by CRLF.
        let next = find_subslice(body, &delimiter, data_start)
            .ok_or_else(|| "multipart 缺少结束边界".to_string())?;
        let mut data_end = next;
        if data_end >= 2 && &body[data_end - 2..data_end] == b"\r\n" {
            data_end -= 2;
        }
        let mut name = String::new();
        let mut filename = None;
        let mut content_type = None;
        for line in headers.lines() {
            let lower = line.to_ascii_lowercase();
            if lower.starts_with("content-disposition:") {
                for segment in line.split(';') {
                    let segment = segment.trim();
                    if let Some(value) = segment.strip_prefix("name=") {
                        name = value.trim_matches('"').to_string();
                    } else if let Some(value) = segment.strip_prefix("filename=") {
                        filename = Some(value.trim_matches('"').to_string());
                    }
                }
            } else if lower.starts_with("content-type:") {
                content_type = Some(line.split(':').nth(1).unwrap_or("").trim().to_string());
            }
        }
        if name.is_empty() {
            return Err("multipart part 缺少 name".to_string());
        }
        parts.push(MultipartPart {
            name,
            filename,
            content_type,
            data: body[data_start..data_end].to_vec(),
        });
        cursor = next;
    }
    Ok(parts)
}

pub fn multipart_boundary(content_type: &str) -> Option<String> {
    content_type.split(';').find_map(|part| {
        let part = part.trim();
        part.strip_prefix("boundary=")
            .map(|b| b.trim().trim_matches('"').to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_and_layered_paths_match_reference_layout() {
        assert_eq!(md5_hex(b"hello"), "5d41402abc4b2a76b9719d911017c592");
        assert_eq!(
            upload_relative_path("5d41402abc4b2a76b9719d911017c592", ".pdf"),
            PathBuf::from("5d/41/5d41402abc4b2a76b9719d911017c592.pdf")
        );
        assert_eq!(
            default_output_relative_path("5d41402abc4b2a76b9719d911017c592"),
            PathBuf::from("temp/5d/41/5d41402abc4b2a76b9719d911017c592.md")
        );
    }

    #[test]
    fn key_masking_never_leaks_more_than_four_chars() {
        assert_eq!(mask_key("sk-test-key-12345678"), "****5678");
        assert_eq!(mask_key("ab"), "****");
        assert!(!mask_key("sk-test-key-12345678").contains("1234"));
    }

    #[test]
    fn unsafe_relative_paths_are_rejected() {
        assert!(safe_relative_path("/etc/passwd").is_none());
        assert!(safe_relative_path("../escape.md").is_none());
        assert!(safe_relative_path("a/../../b").is_none());
        assert_eq!(
            safe_relative_path("docs/./a.md"),
            Some(PathBuf::from("docs/a.md"))
        );
    }

    #[test]
    fn engine_routing_prefers_video_over_audio_like_reference() {
        assert_eq!(engine_for_extension("pdf"), Some("mineru"));
        assert_eq!(engine_for_extension("mp4"), Some("video"));
        assert_eq!(engine_for_extension("mp3"), Some("asr"));
        assert_eq!(engine_for_extension("docx"), Some("anydoc"));
        assert_eq!(engine_for_extension("exe"), None);
        assert_eq!(resolve_task_engine("x/report.pdf", "anydoc"), "mineru");
    }

    #[test]
    fn doc_engine_groups_cover_pdf_office_and_images_only() {
        assert_eq!(doc_engine_group("pdf"), Some("pdf"));
        assert_eq!(doc_engine_group("PDF"), Some("pdf"));
        assert_eq!(doc_engine_group("docx"), Some("office"));
        assert_eq!(doc_engine_group("pptx"), Some("office"));
        assert_eq!(doc_engine_group("epub"), Some("office"));
        assert_eq!(doc_engine_group("png"), Some("image"));
        assert_eq!(doc_engine_group("jpg"), Some("image"));
        assert_eq!(doc_engine_group("txt"), None);
        assert_eq!(doc_engine_group("md"), None);
        assert_eq!(doc_engine_group("mp3"), None);
        assert_eq!(doc_engine_group("mp4"), None);
    }

    #[test]
    fn plain_text_extensions_read_through_without_an_engine() {
        assert!(is_plain_text_extension("txt"));
        assert!(is_plain_text_extension("md"));
        assert!(is_plain_text_extension("markdown"));
        assert!(is_plain_text_extension("rst"));
        assert!(is_plain_text_extension("json"));
        assert!(is_plain_text_extension("xml"));
        assert!(is_plain_text_extension("MD"));
        assert!(!is_plain_text_extension("docx"));
        assert!(!is_plain_text_extension("pdf"));
        assert!(!is_plain_text_extension("html"));
        assert!(!is_plain_text_extension("csv"));
    }

    #[test]
    fn multipart_round_trip() {
        let boundary = "----test-boundary";
        let body = format!(
            "--{b}\r\nContent-Disposition: form-data; name=\"task_id\"\r\n\r\nabc123\r\n--{b}\r\nContent-Disposition: form-data; name=\"files\"; filename=\"a.pdf\"\r\nContent-Type: application/pdf\r\n\r\n%BINARY%\r\n--{b}--\r\n",
            b = boundary
        )
        .replace("%BINARY%", "PDFBYTES");
        let parts = parse_multipart(body.as_bytes(), boundary).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].name, "task_id");
        assert_eq!(parts[0].text(), "abc123");
        assert_eq!(parts[1].filename.as_deref(), Some("a.pdf"));
        assert_eq!(parts[1].data, b"PDFBYTES");
        assert_eq!(
            multipart_boundary("multipart/form-data; boundary=----test-boundary").as_deref(),
            Some("----test-boundary")
        );
    }
}
