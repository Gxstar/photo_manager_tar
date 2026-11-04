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
    let bytes = shell_thumbnail(path)?;
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

/// IShellItemImageFactory → BGRA → JPEG 字节。
/// SIIGBF_THUMBNAILONLY：只取真缩略图（无编解码器时失败而不是给文件图标，
/// 图标由前端画，风格统一）。
fn shell_thumbnail(path: &Path) -> Option<Vec<u8>> {
    unsafe {
        // spawn_blocking 线程池里的线程默认没有初始化 COM
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        let item: IShellItemImageFactory = match SHCreateItemFromParsingName(
            &HSTRING::from(path.as_os_str()),
            None,
        ) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("[thumb] SHCreateItemFromParsingName 失败: {e}");
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
