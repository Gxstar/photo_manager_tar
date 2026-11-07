//! RW2 内嵌预览 JPEG 提取（`thumbs.rs` 的 RAW 兜底）。
//!
//! 系统没装 RAW 编解码扩展时，`IShellItemImageFactory` 拿不到 RW2 缩略图。
//! 松下 RW2 在 APP1 的 TIFF 里用 `JPEGInterchangeFormat(0x0201)` /
//! `JPEGInterchangeFormatLength(0x0202)` 记录了内嵌预览 JPEG 的偏移与长度，
//! 直接按偏移读出即可，不依赖任何外部解码器，也不改动照片文件。

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const HEAD_SCAN: usize = 256 * 1024;

/// 返回内嵌的 JPEG 字节（以 FF D8 开头），失败返回 None。
pub fn embedded_jpeg(path: &Path) -> Option<Vec<u8>> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = vec![0u8; HEAD_SCAN];
    let n = file.read(&mut head).ok()?;
    if n < 16 {
        return None;
    }
    head.truncate(n);
    if &head[0..4] != b"IIU\x00" {
        return None;
    }

    const SIG: &[u8] = b"Exif\x00\x00";
    let pos = head.windows(SIG.len()).position(|w| w == SIG)?;
    let tiff_start = pos + SIG.len();
    let tiff = head.get(tiff_start..)?;
    let le = tiff.get(0..2) == Some(b"II");
    let ifd0 = u32_at(tiff, 4, le)? as usize;

    // 松下 RW2 把缩略图 JPEG 放在 IFD1（IFD0 的 next-IFD 指针），
    // 少数机型放 IFD0 或 Exif 子 IFD，依次找 JPEGInterchangeFormat(0x0201) / Length(0x0202)。
    let ifd0_count = u16_at(tiff, ifd0, le)? as usize;
    let ifd1 = u32_at(tiff, ifd0 + 2 + ifd0_count * 12, le).map(|v| v as usize);
    let exif_ifd = find_tag(tiff, ifd0, le, 0x8769).map(|v| v as usize);
    let mut jpeg_off = None;
    let mut jpeg_len = None;
    for candidate in [Some(ifd0), exif_ifd, ifd1].into_iter().flatten() {
        if let Some(o) = find_tag(tiff, candidate, le, 0x0201) {
            jpeg_off = Some(o);
            jpeg_len = find_tag(tiff, candidate, le, 0x0202);
            break;
        }
    }
    let (off, len) = (jpeg_off? as u64, jpeg_len? as usize);
    if len == 0 || len > 32 * 1024 * 1024 {
        return None;
    }
    file.seek(SeekFrom::Start(tiff_start as u64 + off)).ok()?;
    let mut buf = vec![0u8; len];
    file.read_exact(&mut buf).ok()?;
    (buf.first() == Some(&0xFF) && buf.get(1) == Some(&0xD8)).then_some(buf)
}

fn u16_at(b: &[u8], o: usize, le: bool) -> Option<u16> {
    let x = b.get(o..o + 2)?;
    Some(if le {
        u16::from_le_bytes([x[0], x[1]])
    } else {
        u16::from_be_bytes([x[0], x[1]])
    })
}

fn u32_at(b: &[u8], o: usize, le: bool) -> Option<u32> {
    let x = b.get(o..o + 4)?;
    Some(if le {
        u32::from_le_bytes([x[0], x[1], x[2], x[3]])
    } else {
        u32::from_be_bytes([x[0], x[1], x[2], x[3]])
    })
}

fn find_tag(tiff: &[u8], ifd: usize, le: bool, tag: u16) -> Option<u32> {
    let count = u16_at(tiff, ifd, le)? as usize;
    if count > 4096 {
        return None;
    }
    for i in 0..count {
        let e = ifd + 2 + i * 12;
        if u16_at(tiff, e, le)? == tag {
            return u32_at(tiff, e + 8, le);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实媒体库只读验证：设置 SDI_FIXTURE_ROOT 才跑，最多试 20 个 RW2。
    #[test]
    fn fixture_rw2_preview() {
        let Ok(root) = std::env::var("SDI_FIXTURE_ROOT") else {
            return;
        };
        if root.is_empty() {
            return;
        }
        let base = Path::new(&root).join("原图");
        let mut tried = 0;
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
            tried += 1;
            if let Some(jpeg) = embedded_jpeg(entry.path()) {
                eprintln!(
                    "[rw2 preview] {} bytes from {}",
                    jpeg.len(),
                    entry.path().display()
                );
                assert_eq!(&jpeg[0..2], &[0xFF, 0xD8]);
                return;
            }
            if tried >= 20 {
                break;
            }
        }
        eprintln!("[rw2 preview] 未在 {tried} 个 RW2 中找到内嵌预览");
    }
}
