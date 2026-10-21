use rocket::serde::Serialize;
use std::path::PathBuf;

#[derive(Serialize)]
#[serde(crate = "rocket::serde")]
pub struct File {
    pub dir: Option<PathBuf>,
    pub filename: String,
    pub file_type: FileType,
    pub size: u64,
    pub least_permission: i8,
}

#[derive(Serialize, PartialEq)]
#[serde(crate = "rocket::serde")]
pub enum FileType {
    Dir,
    Code,
    Text,
    Image,
    Music,
    Video,
    Pdf,
    Unknown,
}

impl FileType {
    pub fn infer_file_type(ext: &str) -> Self {
        match ext.to_lowercase().as_str() {
            "c" | "cpp" | "js" | "ts" | "rs" | "py" | "java" | "html" | "css" | "sh" => Self::Code,
            "jpg" | "jpeg" | "gif" | "png" => Self::Image,
            "mp3" | "flac" | "aac" | "ogg" | "wav" => Self::Music,
            "pdf" => Self::Pdf,
            "mp4" | "mov" | "avi" | "mkv" | "webm" | "flv" | "wmv" => Self::Video,
            "txt" | "md" | "srt" | "vtt" | "json" | "yml" | "ini" | "conf" => Self::Text,
            _ => Self::Unknown,
        }
    }
}
