//! JPEG XMP 评分读写（xmp:Rating 标准属性，1-5 星；0 = 未评分）。
//!
//! - 读：扫描文件头 APP1 段找 XMP 包（"http://ns.adobe.com/xap/1.0/\0" 签名），
//!   解析属性式 `xmp:Rating="4"` 或元素式 `<xmp:Rating>4</xmp:Rating>`。
//!   只读头 128KB——XMP 段按规范位于文件头部（EXIF 之后）。
//! - 写：整体重写文件。已有 XMP 段则原地更新属性（保留其余属性），
//!   没有则在头部 APP0/APP1 之后插入新 APP1 段；先写 .tmp 再 rename 原子替换。
//!
//! 范围（用户决策）：评分只覆盖通用图片中的 JPEG（jpg/jpeg）；
//! RAW / 视频 / 其他格式不支持，前端不显示评分 UI。

use std::io::Read;
use std::path::Path;

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

/// 读取 JPEG 的 xmp:Rating（1-5）。None = 无评分 / 非 JPEG / 解析失败。
pub fn jpeg_rating(path: &Path) -> Option<u8> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = vec![0u8; SCAN_LEN];
    let n = f.read(&mut head).ok()?;
    head.truncate(n);
    jpeg_rating_from_head(&head)
}

/// 从文件头字节解析评分（供补齐任务与时间解析共用一次读盘）
pub fn jpeg_rating_from_head(head: &[u8]) -> Option<u8> {
    parse_rating(find_xmp_packet(head)?)
}

/// 写入 xmp:Rating（0 = 清除评分）。返回写入后的错误信息（成功为空）。
pub fn write_jpeg_rating(path: &Path, rating: u8) -> Result<(), String> {
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
    Ok(())
}

/// 在 JPEG 头部段序列中定位 XMP 包（不含命名空间签名）
fn find_xmp_packet(head: &[u8]) -> Option<&[u8]> {
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
                    return Some(&payload[XMP_SIG.len()..]);
                }
                i = seg_end;
            }
            _ => return None, // SOS / SOF：XMP 必在前面，未找到
        }
    }
    None
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

        assert_eq!(jpeg_rating(&p), None, "初始无评分");
        write_jpeg_rating(&p, 4).unwrap();
        assert_eq!(jpeg_rating(&p), Some(4));

        // 再次写入应更新而不是叠加
        write_jpeg_rating(&p, 2).unwrap();
        assert_eq!(jpeg_rating(&p), Some(2));

        // 清除评分 = 写 0
        write_jpeg_rating(&p, 0).unwrap();
        assert_eq!(jpeg_rating(&p), None);

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
        assert_eq!(jpeg_rating(&p), Some(5));
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
