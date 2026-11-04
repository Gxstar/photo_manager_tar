use chrono::NaiveDateTime;
use exif::{Context, In, Reader, Tag};
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

/// 从文件头字节解析拍摄时间（供补齐任务与评分共用一次读盘，IO 减半）。
/// head 为文件前 128KB；解析失败返回 None（调用方可 fallback 全文件解析）。
pub fn photo_time_from_head(head: &[u8]) -> Option<NaiveDateTime> {
    let exif = if head.len() >= 4 && &head[0..4] == b"IIU\x00" {
        // RW2：内嵌 APP1 里的 TIFF 负载
        Reader::new().read_raw(tiff_payload_from_head(head)?).ok()?
    } else if head.len() >= 2 && &head[0..2] == b"\xFF\xD8" {
        // JPEG：头部字节直接作容器解析（EXIF APP1 在头部 128KB 内）
        Reader::new()
            .read_from_container(&mut std::io::Cursor::new(head))
            .ok()?
    } else {
        return None;
    };
    exif_datetime(&exif).map(|(t, _)| t)
}

/// 从文件头字节提取 RW2 内嵌 TIFF 负载
fn tiff_payload_from_head(head: &[u8]) -> Option<Vec<u8>> {
    const SIG: &[u8] = b"Exif\x00\x00";
    let pos = head.windows(SIG.len()).position(|w| w == SIG)?;
    // APP1 段长度位于签名前 2 字节（ff e1 之后，大端），含 "Exif\0\0" 本身
    let seg_len = if pos >= 4 && head[pos - 4] == 0xFF && head[pos - 3] == 0xE1 {
        u16::from_be_bytes([head[pos - 2], head[pos - 1]]) as usize
    } else {
        0
    };
    let tiff_start = pos + SIG.len();
    let tiff_end = if seg_len > SIG.len() + 2 {
        (pos - 4 + 2 + seg_len).min(head.len())
    } else {
        head.len()
    };
    if tiff_start >= tiff_end {
        return None;
    }
    Some(head[tiff_start..tiff_end].to_vec())
}

/// Panasonic RW2：扫描前 128KB 定位 `Exif\0\0` 签名，返回其后 TIFF 负载。
///
/// 实测依据（P1244659.RW2, 27MB）：
/// - 标准 read_from_container 对 RW2 报 InvalidFormat（魔数 IIU\0 非标准 TIFF）
/// - 文件偏移 ~6150 处有完整 APP1 块：ff e1 [len:2] "Exif\0\0" [TIFF...]
/// - 解出的 DateTimeOriginal 与同名 JPG 完全一致
fn rw2_tiff_payload(file: &mut File) -> Option<Vec<u8>> {
    const SCAN_LEN: usize = 128 * 1024;

    let mut head = Vec::with_capacity(SCAN_LEN);
    let mut limited = (&*file).take(SCAN_LEN as u64);
    if limited.read_to_end(&mut head).is_err() {
        return None;
    }
    file.seek(SeekFrom::Start(0)).ok()?;
    tiff_payload_from_head(&head)
}

fn read_rw2_embedded(
    file: &mut File,
) -> Option<Result<(NaiveDateTime, u8), String>> {
    let tiff = rw2_tiff_payload(file)?;

    match Reader::new().read_raw(tiff) {
        Ok(exif) => exif_datetime(&exif).map(Ok),
        Err(e) => Some(Err(format!("RW2 内嵌 EXIF 解析失败: {e}"))),
    }
}

// ── 图库信息栏：完整 EXIF 提取 ────────────────────────────────────────────

/// 单张照片的描述性信息
pub struct PhotoExif {
    pub time: Option<NaiveDateTime>,
    pub make: Option<String>,
    pub model: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// 照片完整信息，分支同 photo_shoot_time（RW2 内嵌 APP1 / 标准容器）
pub fn photo_exif(path: &Path) -> Option<PhotoExif> {
    let mut file = File::open(path).ok()?;
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).ok()?;
    file.seek(SeekFrom::Start(0)).ok()?;

    let is_rw2 = &magic == b"IIU\x00";
    let exif = if is_rw2 {
        let payload = rw2_tiff_payload(&mut file)?;
        Reader::new().read_raw(payload).ok()?
    } else {
        Reader::new()
            .read_from_container(&mut BufReader::new(&mut file))
            .ok()?
    };
    let mut fields = fields_from(&exif);
    if is_rw2 {
        // 传感器尺寸在 RW2 根 IFD0（松下私有 tag），不在内嵌 EXIF 块里
        if let Some((w, h)) = rw2_sensor_dims(&mut file) {
            fields.width = Some(w);
            fields.height = Some(h);
        }
    }
    Some(fields)
}

/// RW2 根 IFD0 的传感器尺寸（松下私有 tag 0x0002/0x0003）。
/// RW2 是魔数 IIU\0 的类 TIFF 结构（非标准 42 魔数，kamadak 无法直接读），
/// 根 IFD0 紧随 8 字节文件头，条目按标准 TIFF 布局：tag(2) type(2) count(4) value(4)。
fn rw2_sensor_dims(file: &mut File) -> Option<(u32, u32)> {
    use std::io::Read;
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut head = [0u8; 8];
    file.read_exact(&mut head).ok()?;
    if &head[0..4] != b"IIU\x00" {
        return None;
    }
    let ifd_off = u32::from_le_bytes(head[4..8].try_into().ok()?) as u64;
    file.seek(SeekFrom::Start(ifd_off)).ok()?;
    let mut cnt = [0u8; 2];
    file.read_exact(&mut cnt).ok()?;
    let count = u16::from_le_bytes(cnt) as usize;

    let mut w = None;
    let mut h = None;
    for _ in 0..count {
        let mut e = [0u8; 12];
        file.read_exact(&mut e).ok()?;
        let tag = u16::from_le_bytes(e[0..2].try_into().ok()?);
        let typ = u16::from_le_bytes(e[2..4].try_into().ok()?);
        let value = match (tag, typ) {
            (0x0002 | 0x0003, 3) => u16::from_le_bytes(e[8..10].try_into().ok()?) as u32,
            (0x0002 | 0x0003, 4) => u32::from_le_bytes(e[8..12].try_into().ok()?),
            _ => continue,
        };
        if tag == 0x0002 {
            w = Some(value);
        } else {
            h = Some(value);
        }
    }
    match (w, h) {
        (Some(w), Some(h)) if w > 0 && h > 0 => Some((w, h)),
        _ => None,
    }
}

fn fields_from(exif: &exif::Exif) -> PhotoExif {
    let text = |tag: Tag| -> Option<String> {
        exif.get_field(tag, In::PRIMARY)
            .map(|f| f.display_value().with_unit(exif).to_string())
            .map(|s| s.trim_matches('\0').trim().to_string())
            .filter(|s| !s.is_empty())
    };
    // 像素尺寸：0xA002/0xA003（ExifImageWidth/Height）在 EXIF sub-IFD，Context 必须用 Exif；
    // 用 Tiff 查不到（JPG 分辨率显示「—」的根因）。RW2 无此 tag，走松下私有
    // 0x0002/0x0003（SensorWidth/Height，根 IFD0，Context::Tiff 正确）。
    let dim = |ctx: Context, tag_num: u16| -> Option<u32> {
        exif.get_field(Tag(ctx, tag_num), In::PRIMARY)
            .and_then(|f| f.value.get_uint(0))
            .map(|v| v as u32)
            .filter(|v| *v > 0)
    };
    let (width, height) = match dim(Context::Exif, 0xA002) {
        Some(w) => (Some(w), dim(Context::Exif, 0xA003)),
        None => (dim(Context::Tiff, 0x0002), dim(Context::Tiff, 0x0003)),
    };
    PhotoExif {
        time: exif_datetime(exif).map(|(t, _)| t),
        make: text(Tag::Make),
        model: text(Tag::Model),
        width,
        height,
    }
}
