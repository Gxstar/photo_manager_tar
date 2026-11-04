//! AVIF / HEIC / HEIF（ISO BMFF 容器）的内嵌 XMP 评分。
//!
//! ISO BMFF 的 XMP 在 `meta → iinf` 里一个 `item_type = "mime"` 的 item，
//! 数据位置由 iloc 的 extent 给出（**绝对文件偏移 + 长度**），
//! 实测位于 mdat 之后（文件尾部），距文件头可达数百 KB。
//! 所以不能「扫文件头找 `<x:xmpmeta`」——必须先解析 iloc 拿到真实偏移再定点读。
//! 要读的只有 meta（前几百字节）和 XMP 载荷（约 1KB）。
//! （JPEG 在 APP1 段，见 `xmp.rs`；厂商 RAW 已明确不支持评分。）
//!
//! 写：「原地改 1 字节」——xmp:Rating 的值恒为单字符（0-5），
//! 替换前后长度相同，容器结构、盒子大小、所有 iloc 偏移、像素数据全不受影响。
//!
//! 实测（本机 1,398 个 AVIF）：全部有 mime item；1,396 个含
//! `<xmp:Rating>0</xmp:Rating>`（值恒为单字符）；2 个无 rating 字段；1 个无 mime item。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
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

/// 能力探测：是否真的存在「可原地改写」的单字符 Rating 位。
/// 与 `write_rating` 的前置条件完全一致（同一套定位逻辑），供信息栏灰显；
/// 无 mime item、无 rating 字段、值不是单字符都返回 false。
pub fn can_write_rating(path: &Path) -> bool {
    let Ok(mut f) = File::open(path) else { return false };
    let Some((abs, len)) = xmp_payload(&mut f) else { return false };
    let Some(payload) = read_at(&mut f, abs, len) else { return false };
    super::xmp::rating_span(&payload)
        .is_some_and(|(s, e)| e - s == 1)
}

/// 原地写入 xmp:Rating：**只改 1 个字节**，文件长度与所有容器偏移不动。
pub fn write_rating(path: &Path, rating: u8) -> Result<(), String> {
    // 与 JPEG 同理：改 XMP 不改画面，mtime 不该变（否则缩略图缓存全失效）
    let orig_mtime = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());

    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("打开失败: {e}"))?;

    let (abs, len) = xmp_payload(&mut f)
        .ok_or_else(|| "未找到内嵌 XMP（AVIF/HEIF 需含 mime 类型的元数据项）".to_string())?;
    let payload = read_at(&mut f, abs, len).ok_or_else(|| "读取 XMP 载荷失败".to_string())?;

    let (vstart, vend) =
        super::xmp::rating_span(&payload).ok_or_else(|| "XMP 里没有 xmp:Rating 字段".to_string())?;

    // 0-5 恒为单字符；长度不符（如 "4.5"）就地替换会改变载荷长度、破坏
    // iloc extent_length → 拒绝，不冒险。
    if vend - vstart != 1 {
        return Err(format!(
            "XMP 评分值为 {} 字节（非 0-5 单字符），不做原地修改以免破坏文件结构",
            vend - vstart
        ));
    }
    let new_byte = b'0' + rating;
    if payload[vstart] == new_byte {
        return Ok(()); // 已是目标值，不产生磁盘写
    }

    f.seek(SeekFrom::Start(abs + vstart as u64))
        .map_err(|e| format!("定位失败: {e}"))?;
    f.write_all(&[new_byte]).map_err(|e| format!("写入失败: {e}"))?;
    f.flush().map_err(|e| format!("刷盘失败: {e}"))?;
    drop(f);

    if let Some(t) = orig_mtime {
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
            let _ = f.set_modified(t);
        }
    }
    Ok(())
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
        let dir = std::env::temp_dir().join(format!("bmff_rating_test_{case}"));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn cleanup(case: &str) {
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!("bmff_rating_test_{case}")));
    }

    #[test]
    fn reads_and_updates_rating() {
        let p = tmp("read", "a.avif", &build_avif(XMP));
        assert!(can_write_rating(&p), "含单字符 Rating 的 AVIF 应可写");
        assert_eq!(rating(&p), None, "0 = 未评分");
        write_rating(&p, 4).unwrap();
        assert_eq!(rating(&p), Some(4));
        write_rating(&p, 0).unwrap();
        assert_eq!(rating(&p), None, "写 0 = 清除评分");
        cleanup("read");
    }

    #[test]
    fn write_changes_exactly_one_byte() {
        let before = build_avif(XMP);
        let p = tmp("onebyte", "b.avif", &before);
        let mtime_before = std::fs::metadata(&p).unwrap().modified().unwrap();

        write_rating(&p, 5).unwrap();

        let after = std::fs::read(&p).unwrap();
        assert_eq!(after.len(), before.len(), "文件长度必须不变");
        let diff: Vec<usize> = before
            .iter()
            .zip(after.iter())
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(diff.len(), 1, "只应有 1 个字节变化");
        assert_eq!(before[diff[0]], b'0');
        assert_eq!(after[diff[0]], b'5');
        assert_eq!(
            std::fs::metadata(&p).unwrap().modified().unwrap(),
            mtime_before,
            "mtime 必须还原（否则缩略图缓存全失效）"
        );
        cleanup("onebyte");
    }

    #[test]
    fn missing_mime_item_is_rejected() {
        let mut data = bx(b"ftyp", b"avif\0\0\0\0mif1avif");
        data.extend(bx(b"mdat", b"nothing here"));
        let p = tmp("nomime", "c.avif", &data);
        assert_eq!(rating(&p), None);
        assert!(!can_write_rating(&p), "无 mime item 应被能力探测判为不可写");
        assert!(write_rating(&p, 3).is_err(), "无 mime item 应明确报错");
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

/// 真实文件冒烟检查（默认忽略）：
///   WB_AVIF="C:\path\copy.avif" cargo test real_avif_smoke -- --ignored --nocapture
/// 用**副本**跑，检查「只改 1 字节 / 长度不变 / mtime 不变 / 评分能读回」。
#[cfg(test)]
mod real_check {
    use super::*;

    #[test]
    #[ignore]
    fn real_avif_smoke() {
        let path = std::env::var("WB_AVIF").expect("需要 WB_AVIF 指向一个 AVIF 副本");
        let p = Path::new(&path);
        let m0 = std::fs::metadata(p).unwrap();
        let before = std::fs::read(p).unwrap();
        let r0 = rating(p);
        println!("写入前: {} 字节, 评分 {:?}", before.len(), r0);

        write_rating(p, 4).unwrap();
        let after = std::fs::read(p).unwrap();
        let m1 = std::fs::metadata(p).unwrap();

        let diff: Vec<usize> = before
            .iter()
            .zip(after.iter())
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .map(|(i, _)| i)
            .collect();
        println!("写入后: 评分 {:?}", rating(p));
        println!("差异字节数 {}, 位置 {:?}", diff.len(), diff);
        for &i in &diff {
            println!("  @{i}: {} -> {}", before[i] as char, after[i] as char);
        }
        println!(
            "长度 {} -> {}（不变={}）",
            before.len(),
            after.len(),
            before.len() == after.len()
        );
        println!(
            "mtime 不变={}",
            m0.modified().unwrap() == m1.modified().unwrap()
        );
        assert_eq!(rating(p), Some(4));
        assert_eq!(diff.len(), 1);
        assert_eq!(before.len(), after.len());
        assert_eq!(m0.modified().unwrap(), m1.modified().unwrap());
    }
}
