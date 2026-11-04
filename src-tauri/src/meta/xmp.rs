//! JPEG / RAW 的 XMP 评分读写（xmp:Rating 标准属性，1-5 星；0 = 未评分）。
//!
//! 两种容器的 XMP 存放位置不同，所以读写方式也不同：
//! - **JPEG**：XMP 在 APP1 段（"http://ns.adobe.com/xap/1.0/\0" 签名）。
//!   写：段长度可能变 → 整体重写文件，tmp + rename 原子替换。
//! - **RAW（TIFF 容器）**：XMP 在 IFD 的 0x02BC (XMLPacket) 指向的数据区，
//!   实测松下 RW2 位于文件头 1323~3651 字节，相机拍时就写好了
//!   `<xmp:Rating>0</xmp:Rating>`。写：值恒为单字符（0-5），**原地改 1 个字节**
//!   ——文件长度、所有 TIFF 偏移、后续数据全不动，比整文件重写安全得多。
//!
//! 读：两者都只读文件头 128KB（与拍摄时间解析共用同一次读盘）。
//!
//! 范围（用户决策）：RAW 评分写文件内嵌 XMP，不生成 sidecar。

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

/// 支持内嵌 XMP 评分的 TIFF 容器 RAW 扩展名。
/// CR3 是 ISO BMFF 容器（XMP 在 box 里），结构不同，暂不列入。
const RAW_EXTS: &[&str] = &["rw2", "dng", "cr2", "nef", "arw", "orf", "raf", "pef"];

/// XMP 命名空间签名（APP1 载荷前缀）
const XMP_SIG: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
/// 只扫描文件头部的字节数
const SCAN_LEN: usize = 128 * 1024;
/// APP1 段载荷上限（len 字段为 u16，含自身 2 字节）
const MAX_SEG: usize = 65533;

/// 该路径是否为可评分的 JPEG（按扩展名判断）
pub fn is_jpeg(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("jpg") | Some("jpeg")
    )
}

/// 该路径是否为 TIFF 容器 RAW（内嵌 XMP 可原地改写）
pub fn is_raw(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .map(|e| RAW_EXTS.contains(&e.as_str()))
        .unwrap_or(false)
}

/// 是否支持评分（JPEG + TIFF 容器 RAW）
pub fn is_ratable(path: &Path) -> bool {
    is_jpeg(path) || is_raw(path)
}

/// 读取 xmp:Rating（1-5）。None = 无评分 / 不支持的格式 / 解析失败。
pub fn rating(path: &Path) -> Option<u8> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = vec![0u8; SCAN_LEN];
    let n = f.read(&mut head).ok()?;
    head.truncate(n);
    rating_from_head(&head)
}

/// 从文件头字节解析评分（JPEG 找 APP1，RAW 找 IFD 数据区里的 XMP 包）。
/// 与拍摄时间解析共用同一次 128KB 读盘。
pub fn rating_from_head(head: &[u8]) -> Option<u8> {
    parse_rating(xmp_in_head(head)?)
}

/// 写入 xmp:Rating（0 = 清除评分）。返回写入后的错误信息（成功为空）。
pub fn write_jpeg_rating(path: &Path, rating: u8) -> Result<(), String> {
    // 记录原 mtime，写入后还原：评分只改 XMP 元数据，画面内容不变。
    // 不还原的后果：文件系统的「修改时间」变成打分时刻（缩略图缓存以 mtime 为
    // key 会全部失效重做，用户看到的照片时间也凭空变了）。拍摄时间在 EXIF 里
    // 本来就不受影响。
    let orig_mtime = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());

    let mut data = std::fs::read(path).map_err(|e| format!("读取失败: {e}"))?;
    if data.len() < 4 || &data[0..2] != b"\xFF\xD8" {
        return Err("不是 JPEG 文件".into());
    }

    // 遍历头部段：定位已有 XMP 段 / 确定插入点（前导 APP0/APP1 之后）
    let mut i = 2usize;
    let mut insert_at = 2usize;
    let mut xmp_range: Option<(usize, usize)> = None; // 覆盖 FF E1 到段尾
    while i < data.len() && data[i] == 0xFF {
        let mut j = i + 1;
        while j < data.len() && data[j] == 0xFF {
            j += 1; // 跳过填充字节
        }
        let Some(&marker) = data.get(j) else { break };
        match marker {
            0xE0..=0xEF => {
                let Some(len_bytes) = data.get(j + 1..j + 3) else { break };
                let len = u16::from_be_bytes([len_bytes[0], len_bytes[1]]) as usize;
                let seg_end = j + 1 + len; // marker(1) + len(2) + payload(len-2)
                if seg_end > data.len() || len < 2 {
                    return Err("JPEG 段结构异常".into());
                }
                let payload = &data[j + 3..seg_end];
                if marker == 0xE1 && payload.starts_with(XMP_SIG) {
                    xmp_range = Some((i, seg_end));
                    break;
                }
                if marker == 0xE0 || marker == 0xE1 {
                    insert_at = seg_end; // XMP 新段插在 JFIF/EXIF 之后
                }
                i = seg_end;
            }
            _ => break, // 到达非 APPn 段（SOF/SOS 等），XMP 探测结束
        }
    }

    let packet = match xmp_range {
        Some((s, e)) => {
            let old = &data[s + 4 + XMP_SIG.len()..e]; // FF E1 len(2) + 签名
            let text = String::from_utf8_lossy(old).to_string();
            update_rating_in_packet(&text, rating)?
        }
        None => fresh_packet(rating),
    };
    if XMP_SIG.len() + packet.len() + 2 > MAX_SEG {
        return Err("XMP 段超出 APP1 容量上限".into());
    }

    let plen = (XMP_SIG.len() + packet.len() + 2) as u16;
    let mut seg = Vec::with_capacity(plen as usize + 2);
    seg.extend_from_slice(&[0xFF, 0xE1]);
    seg.extend_from_slice(&plen.to_be_bytes());
    seg.extend_from_slice(XMP_SIG);
    seg.extend_from_slice(packet.as_bytes());
    match xmp_range {
        Some((s, e)) => {
            data.splice(s..e, seg);
        }
        None => {
            data.splice(insert_at..insert_at, seg);
        }
    }

    // 原子替换：tmp + rename（同目录，保证同盘 rename 原子性）
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let tmp = path.with_file_name(format!("{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, &data).map_err(|e| format!("写入失败: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("替换失败: {e}")
    })?;
    // 还原 mtime（失败不影响评分本身，只是缓存会重做一遍）
    if let Some(t) = orig_mtime {
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
            let _ = f.set_modified(t);
        }
    }
    Ok(())
}

/// 在文件头中定位 XMP 包的字节区间（不含 <?xpacket> 外壳）。
/// JPEG 走 APP1 段遍历；TIFF 容器直接按文本特征定位——不同厂商的 IFD
/// 偏移基数各不相同（RW2 有私有 offset shift），解析 IFD 反而容易出错，
/// 而 XMP 包本身就是带完整 XML 骨架的文本，定位可靠。
fn xmp_packet_range(head: &[u8]) -> Option<(usize, usize)> {
    if head.len() >= 4 && &head[0..2] == b"\xFF\xD8" {
        return jpeg_xmp_range(head);
    }
    let start = find_sub(head, b"<x:xmpmeta")?;
    let rest = &head[start..];
    let end = find_sub(rest, b"</x:xmpmeta>")
        .map(|i| start + i + b"</x:xmpmeta>".len())
        .unwrap_or(head.len());
    Some((start, end))
}

/// 在文件头中取出 XMP 包切片
fn xmp_in_head(head: &[u8]) -> Option<&[u8]> {
    let (s, e) = xmp_packet_range(head)?;
    head.get(s..e)
}

/// JPEG：在头部段序列中定位 XMP 包（跳过命名空间签名），返回 [start, seg_end)
fn jpeg_xmp_range(head: &[u8]) -> Option<(usize, usize)> {
    if head.len() < 4 || &head[0..2] != b"\xFF\xD8" {
        return None;
    }
    let mut i = 2usize;
    while i < head.len() && head[i] == 0xFF {
        let mut j = i + 1;
        while j < head.len() && head[j] == 0xFF {
            j += 1;
        }
        let marker = *head.get(j)?;
        match marker {
            0xE0..=0xEF => {
                let len_bytes = head.get(j + 1..j + 3)?;
                let len = u16::from_be_bytes([len_bytes[0], len_bytes[1]]) as usize;
                if len < 2 {
                    return None;
                }
                let seg_end = j + 1 + len;
                let payload = head.get(j + 3..seg_end)?;
                if marker == 0xE1 && payload.starts_with(XMP_SIG) {
                    return Some((j + 3 + XMP_SIG.len(), seg_end));
                }
                i = seg_end;
            }
            _ => return None, // SOS / SOF：XMP 必在前面，未找到
        }
    }
    None
}

/// 定位 xmp:Rating 值的字节区间（相对文件头的绝对偏移），供原地替换
fn locate_rating_span(head: &[u8]) -> Option<(usize, usize)> {
    const ATTR: &[u8] = b"xmp:Rating=\"";
    const EL: &[u8] = b"<xmp:Rating>";
    const EL_CLOSE: &[u8] = b"</xmp:Rating>";

    let (pkt_start, pkt_end) = xmp_packet_range(head)?;
    let pkt = head.get(pkt_start..pkt_end)?;
    let (vstart, close) = if let Some(p) = find_sub(pkt, ATTR) {
        (pkt_start + p + ATTR.len(), b"\"" as &[u8])
    } else if let Some(p) = find_sub(pkt, EL) {
        (pkt_start + p + EL.len(), EL_CLOSE)
    } else {
        return None;
    };
    let vend = vstart + find_sub(head.get(vstart..pkt_end)?, close)?;
    (vend > vstart).then_some((vstart, vend))
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// 原地写入 RAW 的内嵌 XMP 评分：**只改 1 个字节**。
///
/// 评分恒为单字符（0-5），替换前后长度一致，因此文件大小、所有 TIFF 偏移、
/// 后续像素数据全部不受影响——这是 RAW 评分最安全的形态，无需重写 37MB。
pub fn write_raw_rating(path: &Path, rating: u8) -> Result<(), String> {
    // 与 JPEG 同理：改 XMP 不改画面，mtime 不该变（否则缩略图缓存全失效）
    let orig_mtime = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());

    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("打开失败: {e}"))?;
    let mut head = vec![0u8; SCAN_LEN];
    let n = f.read(&mut head).map_err(|e| format!("读取失败: {e}"))?;
    head.truncate(n);

    let (vstart, vend) = locate_rating_span(&head).ok_or_else(|| {
        "未找到内嵌 XMP 评分（文件不是 TIFF 结构，或相机未写入 xmp:Rating）".to_string()
    })?;

    // 0-5 恒为单字符。长度不符说明是 "4.5" 之类的非常规写法，
    // 原地替换会改变数据区长度并破坏 TIFF 偏移 → 直接拒绝，不冒险。
    if vend - vstart != 1 {
        return Err(format!(
            "XMP 评分值为 {} 字节（非 0-5 单字符），不做原地修改以免破坏文件结构",
            vend - vstart
        ));
    }
    let new_byte = b'0' + rating;
    if head[vstart] == new_byte {
        return Ok(()); // 已是目标值，不产生磁盘写
    }

    f.seek(SeekFrom::Start(vstart as u64)).map_err(|e| format!("定位失败: {e}"))?;
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

/// 从 XMP 包文本解析 xmp:Rating，兼容属性式与元素式；0 / 负值 / 非法 → None
fn parse_rating(packet: &[u8]) -> Option<u8> {
    let s = String::from_utf8_lossy(packet);
    const ATTR: &str = "xmp:Rating=\"";
    const EL_OPEN: &str = "<xmp:Rating>";
    const EL_CLOSE: &str = "</xmp:Rating>";
    let raw = if let Some(pos) = s.find(ATTR) {
        let rest = &s[pos + ATTR.len()..];
        let end = rest.find('"')?;
        &rest[..end]
    } else if let Some(pos) = s.find(EL_OPEN) {
        let rest = &s[pos + EL_OPEN.len()..];
        let end = rest.find(EL_CLOSE)?;
        &rest[..end]
    } else {
        return None;
    };
    // xmp:Rating 是 real 类型，个别工具写小数，四舍五入取整
    let v: f64 = raw.trim().parse().ok()?;
    let rounded = v.round();
    (1.0..=5.0).contains(&rounded).then(|| rounded as u8)
}

/// 更新已有 XMP 包中的 xmp:Rating（保留其余属性）
fn update_rating_in_packet(text: &str, rating: u8) -> Result<String, String> {
    const ATTR: &str = "xmp:Rating=\"";
    const EL_OPEN: &str = "<xmp:Rating>";
    const EL_CLOSE: &str = "</xmp:Rating>";
    let val = rating.to_string();

    // 1) 属性式：xmp:Rating="N"
    if let Some(pos) = text.find(ATTR) {
        let vstart = pos + ATTR.len();
        let vend = text[vstart..]
            .find('"')
            .ok_or("XMP 包结构异常：属性值未闭合")?
            + vstart;
        let mut out = String::with_capacity(text.len() + 2);
        out.push_str(&text[..vstart]);
        out.push_str(&val);
        out.push_str(&text[vend..]);
        return Ok(out);
    }
    // 2) 元素式：<xmp:Rating>N</xmp:Rating>
    if let Some(pos) = text.find(EL_OPEN) {
        let vstart = pos + EL_OPEN.len();
        let vend = text[vstart..]
            .find(EL_CLOSE)
            .ok_or("XMP 包结构异常：元素未闭合")?
            + vstart;
        let mut out = String::with_capacity(text.len() + 2);
        out.push_str(&text[..vstart]);
        out.push_str(&val);
        out.push_str(&text[vend..]);
        return Ok(out);
    }
    // 3) 注入：在第一个 rdf:Description 起始标签内追加属性
    let pos = text.find("<rdf:Description").ok_or("XMP 包缺少 rdf:Description")?;
    let tag_end = text[pos..].find('>').ok_or("XMP 包结构异常")? + pos;
    // xmp 命名空间未声明时一并补上
    let xmlns = if text.contains("xmlns:xmp=") {
        String::new()
    } else {
        " xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\"".to_string()
    };
    let mut out = String::with_capacity(text.len() + xmlns.len() + 20);
    out.push_str(&text[..tag_end]);
    out.push_str(&xmlns);
    out.push_str(&format!(" xmp:Rating=\"{val}\""));
    out.push_str(&text[tag_end..]);
    Ok(out)
}

/// 生成最小 XMP 包（仅含 xmp:Rating）
fn fresh_packet(rating: u8) -> String {
    format!(
        "<?xpacket begin=\"\u{FEFF}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
         <x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"Develop\">\n \
         <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n  \
         <rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\" xmp:Rating=\"{rating}\"/>\n \
         </rdf:RDF>\n\
         </x:xmpmeta>\n\
         <?xpacket end=\"w\"?>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手工拼一个最小 JPEG：SOI + EXIF APP1 + XMP APP1(可选) + SOS 假载荷
    fn build_jpeg(xmp_packet: Option<&str>) -> Vec<u8> {
        let mut d = vec![0xFF, 0xD8];
        let exif_payload: &[u8] = b"Exif\0\0fake-exif-data";
        d.extend_from_slice(&[0xFF, 0xE1]);
        d.extend_from_slice(&((exif_payload.len() + 2) as u16).to_be_bytes());
        d.extend_from_slice(exif_payload);
        if let Some(p) = xmp_packet {
            d.extend_from_slice(&[0xFF, 0xE1]);
            d.extend_from_slice(&((XMP_SIG.len() + p.len() + 2) as u16).to_be_bytes());
            d.extend_from_slice(XMP_SIG);
            d.extend_from_slice(p.as_bytes());
        }
        d.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x0A, 1, 2, 3, 4, 5, 6, 7, 8]);
        d
    }

    fn write_tmp(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn fresh_write_read_roundtrip() {
        let dir = std::env::temp_dir().join("xmp_test_fresh");
        let _ = std::fs::create_dir_all(&dir);
        let p = write_tmp(&dir, "a.jpg", &build_jpeg(None));

        assert_eq!(rating(&p), None, "初始无评分");
        write_jpeg_rating(&p, 4).unwrap();
        assert_eq!(rating(&p), Some(4));

        // 再次写入应更新而不是叠加
        write_jpeg_rating(&p, 2).unwrap();
        assert_eq!(rating(&p), Some(2));

        // 清除评分 = 写 0
        write_jpeg_rating(&p, 0).unwrap();
        assert_eq!(rating(&p), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn existing_xmp_properties_preserved() {
        let dir = std::env::temp_dir().join("xmp_test_keep");
        let _ = std::fs::create_dir_all(&dir);
        let packet = r#"<?xpacket begin="﻿" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:photoshop="http://ns.adobe.com/photoshop/1.0/" photoshop:Credit="gengx"/>
 </rdf:RDF>
</x:xmpmeta>
<?xpacket end="w"?>"#;
        let p = write_tmp(&dir, "b.jpg", &build_jpeg(Some(packet)));

        write_jpeg_rating(&p, 5).unwrap();
        assert_eq!(rating(&p), Some(5));
        // 其余属性保留
        let content = std::fs::read(&p).unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("photoshop:Credit=\"gengx\""), "已有 XMP 属性应保留");
        assert!(text.contains("xmlns:xmp="), "应补声明 xmp 命名空间");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn element_form_and_decimal_rating() {
        let packet = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\"><xmp:Rating>3</xmp:Rating></rdf:Description></rdf:RDF></x:xmpmeta>";
        assert_eq!(parse_rating(packet.as_bytes()), Some(3));
        assert_eq!(parse_rating(b"xmp:Rating=\"4.5\""), Some(5), "小数四舍五入");
        assert_eq!(parse_rating(b"xmp:Rating=\"0\""), None, "0 = 未评分");
        assert_eq!(parse_rating(b"xmp:Rating=\"-1\""), None, "负值 = 拒绝，视为无评分");
        assert_eq!(parse_rating(b"no rating here"), None);
    }

    #[test]
    fn non_jpeg_rejected() {
        let dir = std::env::temp_dir().join("xmp_test_reject");
        let _ = std::fs::create_dir_all(&dir);
        let p = write_tmp(&dir, "c.bin", &[0x49, 0x49, 0x55, 0x00, 1, 2, 3]);
        assert!(write_jpeg_rating(&p, 3).is_err(), "非 JPEG 应拒绝写入");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
