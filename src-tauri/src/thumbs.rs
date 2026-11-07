//! Windows Shell 原生缩略图 + 磁盘缓存。
//!
//! 生成走 `IShellItemImageFactory::GetImage`（与资源管理器同源，自动复用系统
//! 缩略图缓存 thumbcache_*.db），支持照片 / RAW（需系统 RAW 编解码扩展）/
//! 视频首帧。失败返回 None → 协议层回 404 → 前端显示文件图标。

use image::codecs::jpeg::JpegEncoder;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use windows::core::HSTRING;
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    DIB_RGB_COLORS, HBITMAP,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::UI::Shell::{
    SHCreateItemFromParsingName, IShellItemImageFactory, SIIGBF, SIIGBF_BIGGERSIZEOK,
    SIIGBF_THUMBNAILONLY,
};

/// 缩略图边长（shell 按最长边缩放）
const THUMB_SIZE: i32 = 256;

/// 缓存目录：%LOCALAPPDATA%\com.gengx.develop\thumbs
pub fn cache_dir() -> PathBuf {
    std::env::var("LOCALAPPDATA")
        .map(|d| PathBuf::from(d).join("com.gengx.develop").join("thumbs"))
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// 缓存键：固定哈希(路径 | mtime | size)。
/// 哈希算法跨应用版本可能变化 → 最坏情况是缓存全部重生成一次，可接受。
pub fn cache_key(path: &Path, mtime_secs: i64, size_bytes: u64) -> String {
    let mut h = DefaultHasher::new();
    path.to_string_lossy().hash(&mut h);
    mtime_secs.hash(&mut h);
    size_bytes.hash(&mut h);
    format!("{:016x}.jpg", h.finish())
}

/// 只查缓存（协议处理热路径，不碰 COM）
pub fn lookup(path: &Path, mtime_secs: i64, size_bytes: u64) -> Option<Vec<u8>> {
    std::fs::read(cache_dir().join(cache_key(path, mtime_secs, size_bytes))).ok()
}

/// 生成并落缓存（调用方须持有信号量许可，避免 HDD 并发读磁头乱跳）
pub fn create(path: &Path, mtime_secs: i64, size_bytes: u64) -> Option<Vec<u8>> {
    let dir = cache_dir();
    let file = dir.join(cache_key(path, mtime_secs, size_bytes));
    // 双检：等许可期间别的请求可能已生成同一张
    if let Some(bytes) = lookup(path, mtime_secs, size_bytes) {
        return Some(bytes);
    }
    let bytes = shell_thumbnail(path).or_else(|| raw_thumbnail(path))?;
    std::fs::create_dir_all(&dir).ok();
    // 先写临时文件再 rename（Windows 上 rename 原子替换已存在目标），
    // 避免并发请求读到写了一半的 JPEG
    let tmp = file.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        mtime_secs ^ (size_bytes as i64)
    ));
    if std::fs::write(&tmp, &bytes).is_ok() {
        std::fs::rename(&tmp, &file).ok();
    }
    Some(bytes)
}

/// 系统缺 RAW 解码器时的兜底：RW2 内嵌预览 JPEG → 256px → JPEG 缓存。
/// 不依赖 Shell 扩展，不改动照片文件；非 RW2 直接返回 None。
fn raw_thumbnail(path: &Path) -> Option<Vec<u8>> {
    let jpeg = crate::raw_preview::embedded_jpeg(path)?;
    let img = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg).ok()?;
    let thumb = img.thumbnail(256, 256).to_rgb8();
    let mut out = Vec::new();
    thumb
        .write_with_encoder(JpegEncoder::new_with_quality(
            &mut Cursor::new(&mut out),
            82,
        ))
        .ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实 RW2 验证：无系统 RAW 解码器时，内嵌预览仍能生成可用缩略图。
    /// 真实库回归：混合分隔符路径曾让 SHCreateItemFromParsingName 全部失败 (0x80070057)。
    /// 同时按扩展名统计「规范化后 Shell / 最终（含 RW2 兜底）」的可用率。
    /// 需 SDI_FIXTURE_ROOT 指向媒体库根，未设置时静默跳过。
    #[test]
    fn fixture_shell_path_and_coverage() {
        use std::collections::HashMap;
        let Ok(root) = std::env::var("SDI_FIXTURE_ROOT") else {
            return;
        };
        if root.is_empty() {
            return;
        }
        const WANT: &[&str] = &["jpg", "rw2", "avif", "png", "heic", "mp4"];
        const PER: usize = 10;
        let mut picked: HashMap<String, Vec<PathBuf>> = HashMap::new();
        for entry in walkdir::WalkDir::new(Path::new(&root))
            .into_iter()
            .filter_map(Result::ok)
        {
            let p = entry.path();
            let ext = p
                .extension()
                .and_then(|x| x.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if !WANT.contains(&ext.as_str()) {
                continue;
            }
            let v = picked.entry(ext).or_default();
            if v.len() < PER {
                v.push(p.to_path_buf());
            }
            if WANT
                .iter()
                .all(|e| picked.get(*e).is_some_and(|v| v.len() >= PER))
            {
                break;
            }
        }
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            for ext in WANT {
                let Some(files) = picked.get(*ext) else { continue };
                let (mut mixed_ok, mut norm_ok, mut final_ok) = (0, 0, 0);
                let mut err_sample = String::new();
                for p in files {
                    let a: Result<IShellItemImageFactory, _> =
                        SHCreateItemFromParsingName(&HSTRING::from(p.as_os_str()), None);
                    match a {
                        Ok(_) => mixed_ok += 1,
                        Err(e) if err_sample.is_empty() => err_sample = e.to_string(),
                        Err(_) => {}
                    }
                    let b: Result<IShellItemImageFactory, _> =
                        SHCreateItemFromParsingName(&shell_parsing_name(p), None);
                    if b.is_ok() {
                        norm_ok += 1;
                    }
                    if shell_thumbnail(p).or_else(|| raw_thumbnail(p)).is_some() {
                        final_ok += 1;
                    }
                }
                eprintln!(
                    "[thumb diag] .{ext:<5} n={} shell_mixed={mixed_ok} shell_norm={norm_ok} final={final_ok} err={err_sample}",
                    files.len()
                );
            }
        }
    }

    /// 端到端：真实 smart.db 中全部地点相册封面，修复后应 100% 能生成缩略图。
    #[test]
    fn fixture_album_covers_renderable() {
        let Ok(root) = std::env::var("SDI_FIXTURE_ROOT") else {
            return;
        };
        if root.is_empty() {
            return;
        }
        let Ok(local) = std::env::var("LOCALAPPDATA") else {
            return;
        };
        let db = Path::new(&local)
            .join("com.gengx.develop")
            .join("smart.db");
        if !db.is_file() {
            return;
        }
        let Ok(conn) = rusqlite::Connection::open(&db) else {
            return;
        };
        let mut stmt = conn
            .prepare("SELECT cover_path FROM albums WHERE kind='place' AND cover_path IS NOT NULL")
            .unwrap();
        let paths: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .flatten()
            .collect();
        let mut failed = Vec::new();
        for p in &paths {
            let path = Path::new(p);
            if shell_thumbnail(path).or_else(|| raw_thumbnail(path)).is_none() {
                failed.push(p.clone());
            }
        }
        eprintln!(
            "[thumb diag] 相册封面 {}/{} 可生成缩略图",
            paths.len() - failed.len(),
            paths.len()
        );
        for f in failed.iter().take(8) {
            eprintln!("[thumb diag] 失败: {f}");
        }
        assert!(failed.is_empty(), "仍有 {} 个封面取不到缩略图", failed.len());
    }

    #[test]
    fn fixture_rw2_thumb_via_embedded_preview() {
        let Ok(root) = std::env::var("SDI_FIXTURE_ROOT") else {
            return;
        };
        if root.is_empty() {
            return;
        }
        let base = Path::new(&root).join("原图");
        for entry in walkdir::WalkDir::new(base)
            .into_iter()
            .filter_map(Result::ok)
        {
            let ext = entry
                .path()
                .extension()
                .and_then(|x| x.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if ext != "rw2" {
                continue;
            }
            let bytes = raw_thumbnail(entry.path()).expect("RW2 内嵌预览应能生成缩略图");
            assert_eq!(&bytes[0..2], &[0xFF, 0xD8], "输出必须是 JPEG");
            eprintln!("[thumb] RW2 内嵌预览缩略图 {} bytes", bytes.len());
            return;
        }
    }
}

/// Shell 解析名必须是规范的 Win32 路径。
///
/// 媒体库根若配置成 `D:/媒体/图片`，`PathBuf::join` 会拼出 `D:/媒体/图片\原图\…`
/// 这种 `/` 与 `\` 混用的路径。Win32 文件 API 会自行规范化（所以 `metadata` 成功），
/// 但 `SHCreateItemFromParsingName` 拒绝它并返回 E_INVALIDARG (0x80070057)，
/// 表现为大量缩略图 404。同理不能用 `canonicalize`：它带 `\\?\` 前缀，Shell 同样拒绝。
fn shell_parsing_name(path: &Path) -> HSTRING {
    let s = path.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s).replace('/', "\\");
    HSTRING::from(s.as_str())
}

/// IShellItemImageFactory → BGRA → JPEG 字节。
/// SIIGBF_THUMBNAILONLY：只取真缩略图（无编解码器时失败而不是给文件图标，
/// 图标由前端画，风格统一）。
fn shell_thumbnail(path: &Path) -> Option<Vec<u8>> {
    unsafe {
        // spawn_blocking 线程池里的线程默认没有初始化 COM
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        let item: IShellItemImageFactory = match SHCreateItemFromParsingName(
            &shell_parsing_name(path),
            None,
        ) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("[thumb] SHCreateItemFromParsingName 失败: {e} | {}", path.display());
                return None;
            }
        };
        let flags = SIIGBF(SIIGBF_THUMBNAILONLY.0 | SIIGBF_BIGGERSIZEOK.0);
        let hbmp: HBITMAP = match item.GetImage(SIZE { cx: THUMB_SIZE, cy: THUMB_SIZE }, flags) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("[thumb] GetImage 失败: {e}");
                return None;
            }
        };
        let (w, h, bgra) = extract_bgra(hbmp);
        let _ = DeleteObject(hbmp.into());
        let (Some(w), Some(h), Some(bgra)) = (w, h, bgra) else {
            eprintln!("[thumb] 位图像素提取失败");
            return None;
        };

        // BGRA → RGB（shell 位图不透明，alpha 丢弃；JPEG 编码器只收 RGB8）
        let mut rgb = Vec::with_capacity(bgra.len() / 4 * 3);
        for px in bgra.chunks_exact(4) {
            rgb.push(px[2]);
            rgb.push(px[1]);
            rgb.push(px[0]);
        }
        let Some(img) = image::ImageBuffer::<image::Rgb<u8>, Vec<u8>>::from_raw(
            w as u32,
            h as u32,
            rgb,
        ) else {
            eprintln!("[thumb] ImageBuffer 构造失败 {w}x{h}");
            return None;
        };
        let mut out = Vec::with_capacity((w * h) as usize / 2);
        img.write_with_encoder(JpegEncoder::new_with_quality(&mut Cursor::new(&mut out), 82))
            .map_err(|e| eprintln!("[thumb] JPEG 编码失败: {e}"))
            .ok()?;
        Some(out)
    }
}

/// HBITMAP → 32 位 top-down BGRA 像素
fn extract_bgra(hbmp: HBITMAP) -> (Option<i32>, Option<i32>, Option<Vec<u8>>) {
    unsafe {
        let mut bm = BITMAP::default();
        if GetObjectW(
            hbmp.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut _ as *mut _),
        ) == 0
        {
            return (None, None, None);
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
        if w <= 0 || h <= 0 {
            return (None, None, None);
        }
        let hdc = GetDC(None);
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // 负值 = top-down 行序
                biPlanes: 1,
                biBitCount: 32,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; w as usize * h as usize * 4];
        let copied = GetDIBits(
            hdc,
            hbmp,
            0,
            h as u32,
            Some(buf.as_mut_ptr() as *mut _),
            &mut bi,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, hdc);
        if copied == 0 {
            return (None, None, None);
        }
        (Some(w), Some(h), Some(buf))
    }
}
