use crate::models::FileKind;
use std::collections::HashMap;
use std::path::Path;

/// 扫描时直接忽略的系统文件（实测 原图\ 与 视频\ 根目录各有 1 个 desktop.ini）
pub const IGNORED_NAMES: &[&str] = &["desktop.ini", "thumbs.db", ".ds_store"];

pub fn is_ignored_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    IGNORED_NAMES.iter().any(|n| lower == *n)
}

pub fn ext_of(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// 扩展名 → 类别
pub fn classify(ext: &str) -> FileKind {
    match ext {
        "mp4" | "mov" | "m4v" | "mts" | "m2ts" | "avi" | "mkv" | "3gp" => FileKind::Video,
        "rw2" | "dng" | "cr2" | "cr3" | "nef" | "arw" | "orf" | "raf" | "pef" => FileKind::Raw,
        "jpg" | "jpeg" | "png" | "heic" | "heif" | "avif" | "tif" | "tiff" | "webp" | "bmp"
        | "gif" => FileKind::Image,
        "xmp" | "acr" | "pp3" | "rrdata" | "aae" => FileKind::Sidecar,
        _ => FileKind::Unknown,
    }
}

/// sidecar 关联：在同目录文件名表中为 sidecar 找主文件。
/// 实测三种命名形态都必须命中：
///   P1161462.xmp            → 剥 1 层得 P1161462 → 命中 P1161462.JPG
///   P1160211.RW2.xmp        → 剥 1 层得 P1160211.RW2 → 精确命中
///   IMG_20260329_123640.dng.xmp → 剥 1 层得 IMG_20260329_123640.dng → 精确命中
///
/// dir_files: 目录内全部文件（name_lowercase → FileMeta）
pub fn find_main_file<'a>(
    dir_files: &'a HashMap<String, DirFileMeta>,
    sidecar_name: &str,
) -> Option<&'a DirFileMeta> {
    let mut cur = sidecar_name.to_string();
    for _ in 0..3 {
        let (stem, _) = cur.rsplit_once('.')?;
        cur = stem.to_string();
        let lower = cur.to_lowercase();
        // 精确命中（候选本身就是主文件全名，如 P1160211.RW2）
        if let Some(m) = dir_files.get(&lower) {
            if m.kind != FileKind::Sidecar {
                return Some(m);
            }
        }
        // stem + 媒体扩展名（如 P1161462 + .jpg）
        for ext in ["jpg", "jpeg", "png", "heic", "heif", "avif", "tif", "tiff", "webp",
                    "rw2", "dng", "cr2", "cr3", "nef", "arw", "orf", "raf", "pef",
                    "mp4", "mov", "m4v"] {
            if let Some(m) = dir_files.get(&format!("{lower}.{ext}")) {
                return Some(m);
            }
        }
    }
    None
}

/// 目录内文件元数据（用于 sidecar 关联）
#[derive(Debug, Clone)]
pub struct DirFileMeta {
    pub file_name: String,
    pub kind: FileKind,
}
