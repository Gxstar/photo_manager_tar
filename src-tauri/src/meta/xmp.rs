//! JPEG 内嵌 XMP 评分的**只读**解析（docs/开发方案.md §10）。
//!
//! 2026-11-05 起评分权威源是 SQLite `ratings` 表，本模块**不再写入任何图片文件**。
//! 保留的能力只用于「一次性从文件导入旧评分」：JPEG 的 XMP 在 APP1 段
//! （`http://ns.adobe.com/xap/1.0/\0` 签名），读文件头 128KB 即可定位。
//! ISO BMFF（AVIF/HEIC）的内嵌 XMP 在 `bmff.rs`；sidecar 的生成在 `sidecar.rs`。

use std::io::Read;
use std::path::Path;

/// XMP 命名空间签名（APP1 载荷前缀）
const XMP_SIG: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
/// 只扫描文件头部的字节数
const SCAN_LEN: usize = 128 * 1024;

/// 读取内嵌 xmp:Rating（1-5）。None = 无评分 / 不是 JPEG / 解析失败。
pub fn rating(path: &Path) -> Option<u8> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = vec![0u8; SCAN_LEN];
    let n = f.read(&mut head).ok()?;
    head.truncate(n);
    rating_from_head(&head)
}

/// 从文件头字节解析评分（JPEG 找 APP1 段）
pub fn rating_from_head(head: &[u8]) -> Option<u8> {
    parse_rating(xmp_in_head(head)?)
}

/// 在 JPEG 文件头中定位 XMP 包的字节区间（不含 <?xpacket> 外壳）
fn xmp_in_head(head: &[u8]) -> Option<&[u8]> {
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
                    return head.get(j + 3 + XMP_SIG.len()..seg_end);
                }
                i = seg_end;
            }
            _ => return None, // SOS / SOF：XMP 必在前面，未找到
        }
    }
    None
}

/// 从 XMP 包文本解析 xmp:Rating，兼容属性式与元素式；0 / 负值 / 非法 → None
pub(crate) fn parse_rating(packet: &[u8]) -> Option<u8> {
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

    #[test]
    fn reads_embedded_rating_read_only() {
        let dir = std::env::temp_dir().join("xmp_readonly_test");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("a.jpg");
        let packet = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\"><xmp:Rating>3</xmp:Rating></rdf:Description></rdf:RDF></x:xmpmeta>";
        std::fs::write(&p, build_jpeg(Some(packet))).unwrap();
        let before = std::fs::read(&p).unwrap();
        assert_eq!(rating(&p), Some(3));
        assert_eq!(std::fs::read(&p).unwrap(), before, "读取绝不能改文件");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn element_form_and_decimal_rating() {
        let packet = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\"><xmp:Rating>3</xmp:Rating></rdf:Description></rdf:RDF></x:xmpmeta>";
        assert_eq!(parse_rating(packet.as_bytes()), Some(3));
        assert_eq!(parse_rating(b"xmp:Rating=\"4.5\""), Some(5), "小数四舍五入");
        assert_eq!(parse_rating(b"xmp:Rating=\"0\""), None, "0 = 未评分");
        assert_eq!(parse_rating(b"xmp:Rating=\"-1\""), None, "负值 = 拒绝，视为无评分");
        assert_eq!(parse_rating(b"xmp:Rating=\"6\""), None, "超出 1-5 不接受");
        assert_eq!(parse_rating(b"no rating here"), None);
    }
}
