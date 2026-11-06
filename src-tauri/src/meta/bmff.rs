//! AVIF / HEIC / HEIF（ISO BMFF 容器）内嵌 XMP 的**只读**解析。
//!
//! ISO BMFF 的 XMP 在 `meta → iinf` 里一个 `item_type = "mime"` 的 item，
//! 数据位置由 iloc 的 extent 给出（**绝对文件偏移 + 长度**），
//! 实测位于 mdat 之后（文件尾部），距文件头可达数百 KB。
//! 所以不能「扫文件头找 `<x:xmpmeta`」——必须先解析 iloc 拿到真实偏移再定点读。
//! 要读的只有 meta（前几百字节）和 XMP 载荷（约 1KB）。
//!
//! 2026-11-05 起本模块**不再写入**：评分权威源是 SQLite，保留解析只用于
//! 「一次性从文件导入旧评分」。文件永远以只读方式打开。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// 支持 ISO BMFF 内嵌 XMP 的扩展名。
/// CR3 也是 BMFF，但 Canon 的 XMP 放在 uuid/CMT 盒里且常无 mime item，未验证前不列入。
const BMFF_EXTS: &[&str] = &["avif", "heic", "heif"];

/// 首次读取文件头的长度（meta 一般在最前面，几百字节即可覆盖）
const HEAD_FIRST: usize = 64 * 1024;
/// meta 异常大时的兜底读取长度
const HEAD_RETRY: usize = 4 * 1024 * 1024;
/// XMP 载荷读取上限（实测约 1KB；异常文件不无限读）
const PAYLOAD_MAX: u32 = 256 * 1024;

/// 该路径是否为 ISO BMFF 容器图片（按扩展名判断）
pub fn is_bmff(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .map(|e| BMFF_EXTS.contains(&e.as_str()))
        .unwrap_or(false)
}

/// 读取 xmp:Rating（1-5）。None = 无评分 / 无 XMP / 解析失败。
pub fn rating(path: &Path) -> Option<u8> {
    let mut f = File::open(path).ok()?;
    let (abs, len) = xmp_payload(&mut f)?;
    let payload = read_at(&mut f, abs, len)?;
    super::xmp::parse_rating(&payload)
}

/// 定位 XMP 载荷：(绝对文件偏移, 长度)。
/// 失败 = 不是 BMFF / 没有 meta / 没有 mime item / 非「单 extent + 绝对偏移」形态。
fn xmp_payload(f: &mut File) -> Option<(u64, u32)> {
    let file_len = f.metadata().ok()?.len();
    for cap in [HEAD_FIRST, HEAD_RETRY] {
        let want = (cap as u64).min(file_len) as u32;
        let head = read_at(f, 0, want)?;
        if let Some(span) = locate_in_head(&head) {
            return Some(span);
        }
        if head.len() as u64 >= file_len {
            break; // 已读完整个文件，不必再试
        }
    }
    None
}

/// 在文件头字节里解析盒子树，返回 mime item 的数据区间
fn locate_in_head(head: &[u8]) -> Option<(u64, u32)> {
    let meta = boxes(head, 0, head.len()).into_iter().find(|b| &b.typ == b"meta")?;

    // meta 是 FullBox：子盒比 payload 起点再晚 4 字节
    let children = boxes(head, meta.payload + 4, meta.end);
    let iinf = children.iter().find(|b| &b.typ == b"iinf")?;
    let iloc = children.iter().find(|b| &b.typ == b"iloc")?;

    let mime_id = mime_item_id(head, iinf)?;
    let hits: Vec<_> = iloc_extents(head, iloc)?
        .into_iter()
        .filter(|e| e.item_id == mime_id)
        .collect();
    // 实测 1,398 个 AVIF 全部是「单个 item、单 extent、绝对偏移」；
    // 其他形态（idat 相对偏移 / 多 extent）直接放弃而不是猜。
    if hits.len() != 1 {
        return None;
    }
    let e = &hits[0];
    if e.method != 0 || e.data_ref != 0 || e.extents.len() != 1 {
        return None;
    }
    let (off, len) = e.extents[0];
    let len = u32::try_from(len).ok()?;
    if len == 0 || len > PAYLOAD_MAX {
        return None;
    }
    Some((off, len))
}

/// 遍历 [start, end) 区间内的盒子；遇到非法尺寸即停止（不 panic）
fn boxes(buf: &[u8], start: usize, end: usize) -> Vec<BoxRef> {
    let mut out = Vec::new();
    let mut p = start;
    let end = end.min(buf.len());
    while p + 8 <= end {
        let size32 = u32::from_be_bytes([buf[p], buf[p + 1], buf[p + 2], buf[p + 3]]);
        let typ = [buf[p + 4], buf[p + 5], buf[p + 6], buf[p + 7]];
        let (size, hdr) = match size32 {
            0 => ((end - p) as u64, 8usize), // 延伸到末尾
            1 => {
                if p + 16 > end {
                    break;
                }
                let mut b = [0u8; 8];
                b.copy_from_slice(&buf[p + 8..p + 16]);
                (u64::from_be_bytes(b), 16usize)
            }
            n => (n as u64, 8usize),
        };
        if size < hdr as u64 || p as u64 + size > end as u64 {
            break;
        }
        out.push(BoxRef { typ, payload: p + hdr, end: (p as u64 + size) as usize });
        p = (p as u64 + size) as usize;
    }
    out
}

struct BoxRef {
    typ: [u8; 4],
    payload: usize,
    end: usize,
}

/// iinf → infe 里 item_type == "mime" 的 item_id
fn mime_item_id(buf: &[u8], iinf: &BoxRef) -> Option<u32> {
    let version = *buf.get(iinf.payload)?;
    let mut p = iinf.payload + 4; // version + flags
    p += if version == 0 { 2 } else { 4 }; // entry_count
    for b in boxes(buf, p, iinf.end) {
        if &b.typ != b"infe" {
            continue;
        }
        let v = *buf.get(b.payload)?;
        let (id, type_at) = match v {
            // 旧式 infe 没有 item_type 字段，不可能承载 mime
            0 | 1 => continue,
            2 => (
                u16::from_be_bytes([*buf.get(b.payload + 4)?, *buf.get(b.payload + 5)?]) as u32,
                b.payload + 8,
            ),
            _ => (
                u32::from_be_bytes([
                    *buf.get(b.payload + 4)?,
                    *buf.get(b.payload + 5)?,
                    *buf.get(b.payload + 6)?,
                    *buf.get(b.payload + 7)?,
                ]),
                b.payload + 10,
            ),
        };
        if buf.get(type_at..type_at + 4) == Some(b"mime".as_slice()) {
            return Some(id);
        }
    }
    None
}

struct IlocEntry {
    item_id: u32,
    method: u16,
    data_ref: u16,
    /// (绝对偏移, 长度)
    extents: Vec<(u64, u64)>,
}

/// iloc → 每个 item 的 extent 列表
fn iloc_extents(buf: &[u8], iloc: &BoxRef) -> Option<Vec<IlocEntry>> {
    let version = *buf.get(iloc.payload)?;
    let p = iloc.payload + 4;
    let sizes = u16::from_be_bytes([*buf.get(p)?, *buf.get(p + 1)?]);
    let offset_size = (sizes >> 12) & 0xf;
    let length_size = (sizes >> 8) & 0xf;
    let base_size = (sizes >> 4) & 0xf;
    let index_size = if version >= 1 { sizes & 0xf } else { 0 };

    let mut p = p + 2;
    let count = if version < 2 {
        let c = u16::from_be_bytes([*buf.get(p)?, *buf.get(p + 1)?]);
        p += 2;
        c as usize
    } else {
        let c = u32::from_be_bytes([
            *buf.get(p)?,
            *buf.get(p + 1)?,
            *buf.get(p + 2)?,
            *buf.get(p + 3)?,
        ]);
        p += 4;
        c as usize
    };

    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let item_id = if version < 2 {
            let v = u16::from_be_bytes([*buf.get(p)?, *buf.get(p + 1)?]) as u32;
            p += 2;
            v
        } else {
            let v = u32::from_be_bytes([
                *buf.get(p)?,
                *buf.get(p + 1)?,
                *buf.get(p + 2)?,
                *buf.get(p + 3)?,
            ]);
            p += 4;
            v
        };
        let mut method = 0u16;
        if version >= 1 {
            method = u16::from_be_bytes([*buf.get(p)?, *buf.get(p + 1)?]) & 0xf;
            p += 2;
        }
        let data_ref = u16::from_be_bytes([*buf.get(p)?, *buf.get(p + 1)?]);
        p += 2;
        let base = read_uint(buf, p, base_size as usize)?;
        p += base_size as usize;
        let extent_count = u16::from_be_bytes([*buf.get(p)?, *buf.get(p + 1)?]) as usize;
        p += 2;
        let mut extents = Vec::with_capacity(extent_count);
        for _ in 0..extent_count {
            if index_size > 0 {
                p += index_size as usize;
            }
            let off = read_uint(buf, p, offset_size as usize)?;
            p += offset_size as usize;
            let len = read_uint(buf, p, length_size as usize)?;
            p += length_size as usize;
            extents.push((base + off, len));
        }
        out.push(IlocEntry { item_id, method, data_ref, extents });
    }
    Some(out)
}

/// 读取 n 字节的大端无符号整数（iloc 字段宽度可为 0/2/4/8）
fn read_uint(buf: &[u8], at: usize, n: usize) -> Option<u64> {
    let s = buf.get(at..at + n)?;
    Some(s.iter().fold(0u64, |v, b| (v << 8) | *b as u64))
}

/// 定点读取文件片段
fn read_at(f: &mut File, off: u64, len: u32) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; len as usize];
    f.seek(SeekFrom::Start(off)).ok()?;
    let mut read = 0usize;
    while read < buf.len() {
        let n = f.read(&mut buf[read..]).ok()?;
        if n == 0 {
            break;
        }
        read += n;
    }
    buf.truncate(read);
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(typ);
        v.extend_from_slice(body);
        v
    }

    /// 拼一个最小 AVIF：ftyp + meta(hdlr/pitm/iinf/iloc) + mdat(XMP)。
    /// 复刻实测结构：iloc v0 / offset_size=4 / length_size=4 / base_size=0。
    fn build_avif(payload: &str) -> Vec<u8> {
        let mut meta = vec![0u8, 0, 0, 0]; // meta 是 FullBox
        meta.extend(bx(b"hdlr", &[0u8; 25]));

        let mut pitm = vec![0u8, 0, 0, 0];
        pitm.extend(1u16.to_be_bytes());
        meta.extend(bx(b"pitm", &pitm));

        let mut infe = vec![2u8, 0, 0, 0]; // infe v2
        infe.extend(1u16.to_be_bytes()); // item_id
        infe.extend(0u16.to_be_bytes()); // protection
        infe.extend_from_slice(b"mime");
        infe.extend_from_slice(b"\0"); // item_name
        infe.extend_from_slice(b"application/rdf+xml\0"); // content_type
        infe.extend_from_slice(b"\0"); // content_encoding
        let mut iinf = vec![0u8, 0, 0, 0];
        iinf.extend(1u16.to_be_bytes()); // entry_count
        iinf.extend(bx(b"infe", &infe));
        meta.extend(bx(b"iinf", &iinf));

        // iloc：extent_offset 先填 0，拼完后回填真实偏移
        let mut iloc = vec![0u8, 0, 0, 0];
        iloc.extend(0x4400u16.to_be_bytes()); // offset=4 / length=4 / base=0
        iloc.extend(1u16.to_be_bytes()); // item_count
        iloc.extend(1u16.to_be_bytes()); // item_id
        iloc.extend(0u16.to_be_bytes()); // data_ref
        iloc.extend(1u16.to_be_bytes()); // extent_count
        iloc.extend(0u32.to_be_bytes()); // extent_offset ← 回填
        iloc.extend((payload.len() as u32).to_be_bytes());
        let iloc_len = iloc.len();
        meta.extend(bx(b"iloc", &iloc));

        let ftyp = bx(b"ftyp", b"avif\0\0\0\0mif1avif");
        let meta_box = bx(b"meta", &meta);
        // mdat 载荷起点 = ftyp + meta + mdat 头
        let data_off = (ftyp.len() + meta_box.len() + 8) as u32;

        // extent_offset 在整文件中的下标：
        //   ftyp + meta 头(8) + meta fullbox(4) + 其他子盒 + iloc 头(8)
        //   + iloc payload 内偏移(fullbox4+sizes2+count2+id2+ref2+extent_count2 = 14)
        let other_boxes = meta.len() - 4 - iloc_len - 8;
        let at = ftyp.len() + 8 + 4 + other_boxes + 8 + 14;

        let mut file = Vec::new();
        file.extend_from_slice(&ftyp);
        file.extend_from_slice(&meta_box);
        file[at..at + 4].copy_from_slice(&data_off.to_be_bytes());
        file.extend(bx(b"mdat", payload.as_bytes()));
        file
    }

    const XMP: &str = "<?xpacket begin=\"\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?><x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\"><xmp:Rating>0</xmp:Rating></rdf:Description></rdf:RDF></x:xmpmeta><?xpacket end=\"w\"?>";

    /// 每个用例独立的临时目录（用例并行跑，共用目录会互删）
    fn tmp(case: &str, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("bmff_read_test_{case}"));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn cleanup(case: &str) {
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!("bmff_read_test_{case}")));
    }

    #[test]
    fn reads_embedded_rating_without_writing() {
        let with4 = XMP.replace("<xmp:Rating>0</xmp:Rating>", "<xmp:Rating>4</xmp:Rating>");
        let bytes = build_avif(&with4);
        let p = tmp("read", "a.avif", &bytes);
        assert_eq!(rating(&p), Some(4));
        assert_eq!(std::fs::read(&p).unwrap(), bytes, "读取绝不能改文件");

        let p0 = tmp("read", "zero.avif", &build_avif(XMP));
        assert_eq!(rating(&p0), None, "0 = 未评分");
        cleanup("read");
    }

    #[test]
    fn missing_mime_item_returns_none() {
        let mut data = bx(b"ftyp", b"avif\0\0\0\0mif1avif");
        data.extend(bx(b"mdat", b"nothing here"));
        let p = tmp("nomime", "c.avif", &data);
        assert_eq!(rating(&p), None);
        cleanup("nomime");
    }

    #[test]
    fn ext_detection() {
        assert!(is_bmff(Path::new("a.AVIF")));
        assert!(is_bmff(Path::new("b.heic")));
        assert!(!is_bmff(Path::new("c.jpg")));
        assert!(!is_bmff(Path::new("d.rw2")));
    }
}
