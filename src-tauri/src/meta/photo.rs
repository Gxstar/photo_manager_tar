use chrono::NaiveDateTime;
use exif::{In, Reader, Tag};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

/// 照片拍摄时间解析入口。
/// 分支：RW2（松下私有魔数 IIU\0）走内嵌 APP1 提取；其余走标准容器解析。
pub fn photo_shoot_time(
    path: &Path,
) -> Option<Result<(NaiveDateTime, u8), String>> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return None,
    };

    // 读魔数判断格式
    let mut magic = [0u8; 4];
    if file.read_exact(&mut magic).is_err() {
        return None;
    }
    file.seek(SeekFrom::Start(0)).ok()?;

    if &magic == b"IIU\x00" {
        // Panasonic RW2：内嵌 JPEG 式 APP1 块
        match read_rw2_embedded(&mut file) {
            Some(Ok(t)) => Some(Ok(t)),
            Some(Err(e)) => Some(Err(e)),
            None => None,
        }
    } else {
        // JPEG / TIFF / PNG / WebP / HEIF / AVIF 等标准容器
        let mut reader = BufReader::new(file);
        match Reader::new().read_from_container(&mut reader) {
            Ok(exif) => exif_datetime(&exif).map(Ok),
            Err(e) => Some(Err(format!("EXIF 解析失败: {e}"))),
        }
    }
}

/// 从 Exif 数据中按优先级取时间：DateTimeOriginal → DateTimeDigitized → DateTime
fn exif_datetime(exif: &exif::Exif) -> Option<(NaiveDateTime, u8)> {
    let try_tag = |tag: Tag| -> Option<(NaiveDateTime, u8)> {
        let field = exif.get_field(tag, In::PRIMARY)?;
        let s = field.display_value().with_unit(exif).to_string();
        // 值形如 "2026-10-02 20:42:37"（display 已把冒号格式转为连字符）
        NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S")
            .ok()
            .map(|t| (t, 1))
    };
    try_tag(Tag::DateTimeOriginal)
        .or_else(|| try_tag(Tag::DateTimeDigitized))
        .or_else(|| try_tag(Tag::DateTime))
}

/// Panasonic RW2：扫描前 128KB 定位 `Exif\0\0` 签名，
/// 把其后的 TIFF 负载交给 read_raw 解析。
///
/// 实测依据（P1244659.RW2, 27MB）：
/// - 标准 read_from_container 对 RW2 报 InvalidFormat（魔数 IIU\0 非标准 TIFF）
/// - 文件偏移 ~6150 处有完整 APP1 块：ff e1 [len:2] "Exif\0\0" [TIFF...]
/// - 解出的 DateTimeOriginal 与同名 JPG 完全一致
fn read_rw2_embedded(
    file: &mut File,
) -> Option<Result<(NaiveDateTime, u8), String>> {
    const SIG: &[u8] = b"Exif\x00\x00";
    const SCAN_LEN: usize = 128 * 1024;

    let mut head = Vec::with_capacity(SCAN_LEN);
    let mut limited = (&*file).take(SCAN_LEN as u64);
    if limited.read_to_end(&mut head).is_err() {
        return None;
    }

    let pos = head.windows(SIG.len()).position(|w| w == SIG)?;
    // APP1 段长度位于签名前 2 字节（ff e1 之后，大端），含 "Exif\0\0" 本身
    let seg_len = if pos >= 4 && head[pos - 4] == 0xFF && head[pos - 3] == 0xE1 {
        u16::from_be_bytes([head[pos - 2], head[pos - 1]]) as usize
    } else {
        0
    };
    // TIFF 负载起点与长度
    let tiff_start = pos + SIG.len();
    let tiff_end = if seg_len > SIG.len() + 2 {
        (pos - 4 + 2 + seg_len).min(head.len())
    } else {
        head.len()
    };
    if tiff_start >= tiff_end {
        return None;
    }

    let tiff = head[tiff_start..tiff_end].to_vec();
    file.seek(SeekFrom::Start(0)).ok()?;

    match Reader::new().read_raw(tiff) {
        Ok(exif) => exif_datetime(&exif).map(Ok),
        Err(e) => Some(Err(format!("RW2 内嵌 EXIF 解析失败: {e}"))),
    }
}
