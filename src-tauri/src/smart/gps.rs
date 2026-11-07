//! EXIF GPS 只读解析（docs §21）。
//!
//! 与 `meta/photo.rs` 同一套读法：只读文件头，不整文件加载，HDD 友好。
//! JPEG / TIFF / PNG / WebP 走标准容器；RW2（松下私有魔数 IIU\0）走内嵌 APP1 TIFF。
//! GPS 字段在 GPS IFD，但 `kamadak-exif` 把子 IFD 合并进 `In::PRIMARY`，
//! 官方单测即以 `get_field(Tag::GPSLatitude, In::PRIMARY)` 取值。

use exif::{In, Reader, Tag, Value};
use std::io::{Cursor, Read};
use std::path::Path;

/// EXIF 在头部；256KB 比 photo.rs 的 128KB 放宽一档，容纳较大的 APP1。
const HEAD_SCAN: usize = 256 * 1024;

/// 读文件头解析 GPS，返回 WGS-84 十进制度 (lat, lon)。无 GPS / 不支持的格式返回 None。
pub fn read_gps(path: &Path) -> Option<(f64, f64)> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = vec![0u8; HEAD_SCAN];
    let n = file.read(&mut head).ok()?;
    if n < 4 {
        return None;
    }
    head.truncate(n);

    let exif = if &head[0..4] == b"IIU\x00" {
        Reader::new().read_raw(tiff_payload(&head)?).ok()?
    } else {
        Reader::new()
            .read_from_container(&mut Cursor::new(head.as_slice()))
            .ok()?
    };

    let lat = dms(exif.get_field(Tag::GPSLatitude, In::PRIMARY).map(|f| &f.value))?;
    let lon = dms(exif.get_field(Tag::GPSLongitude, In::PRIMARY).map(|f| &f.value))?;
    let lat_ref = ascii_ref(exif.get_field(Tag::GPSLatitudeRef, In::PRIMARY).map(|f| &f.value));
    let lon_ref = ascii_ref(exif.get_field(Tag::GPSLongitudeRef, In::PRIMARY).map(|f| &f.value));
    let lat = if lat_ref.as_deref() == Some("S") { -lat } else { lat };
    let lon = if lon_ref.as_deref() == Some("W") { -lon } else { lon };

    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
}

/// 度分秒 → 十进制度。分母为 0 的坏数据按 0 处理，不 panic。
fn dms(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Rational(r)) if r.len() >= 3 => {
            let ratio = |num: u32, den: u32| {
                if den == 0 {
                    0.0
                } else {
                    num as f64 / den as f64
                }
            };
            Some(
                ratio(r[0].num, r[0].denom)
                    + ratio(r[1].num, r[1].denom) / 60.0
                    + ratio(r[2].num, r[2].denom) / 3600.0,
            )
        }
        _ => None,
    }
}

fn ascii_ref(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::Ascii(a)) => a
            .first()
            .map(|b| String::from_utf8_lossy(b).trim().to_ascii_uppercase()),
        _ => None,
    }
}

/// 从头部字节提取 RW2 内嵌 TIFF 负载（与 `meta/photo.rs::tiff_payload_from_head` 同口径）。
fn tiff_payload(head: &[u8]) -> Option<Vec<u8>> {
    const SIG: &[u8] = b"Exif\x00\x00";
    let pos = head.windows(SIG.len()).position(|w| w == SIG)?;
    let seg_len = if pos >= 4 && head[pos - 4] == 0xFF && head[pos - 3] == 0xE1 {
        u16::from_be_bytes([head[pos - 2], head[pos - 1]]) as usize
    } else {
        0
    };
    let start = pos + SIG.len();
    let end = if seg_len > SIG.len() + 2 {
        (pos - 4 + 2 + seg_len).min(head.len())
    } else {
        head.len()
    };
    (start < end).then(|| head[start..end].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dms_converts() {
        let v = Value::Rational(vec![
            (30u32, 1u32).into(),
            (15u32, 1u32).into(),
            (0u32, 1u32).into(),
        ]);
        assert!((dms(Some(&v)).unwrap() - 30.25).abs() < 1e-9);
    }

    #[test]
    fn bad_rational_is_rejected() {
        let v = Value::Rational(vec![(1u32, 1u32).into()]);
        assert!(dms(Some(&v)).is_none());
        assert!(dms(None).is_none());
    }

    #[test]
    fn ref_sign() {
        let v = Value::Ascii(vec![b"S".to_vec()]);
        assert_eq!(ascii_ref(Some(&v)).as_deref(), Some("S"));
    }

    /// 真实媒体库只读冒烟测试：设置 SDI_FIXTURE_ROOT 才跑（与项目 tests.rs 同惯例），
    /// 上限 1500 个文件，只统计不写盘，用 --nocapture 看结果。
    #[test]
    fn fixture_scan_smoke() {
        let Ok(root) = std::env::var("SDI_FIXTURE_ROOT") else {
            return;
        };
        if root.is_empty() {
            return;
        }
        let base = Path::new(&root).join("原图");
        if !base.is_dir() {
            return;
        }
        let (mut seen, mut jpg_gps, mut rw2_gps, mut rw2) = (0, 0, 0, 0);
        for entry in walkdir::WalkDir::new(&base)
            .into_iter()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let ext = entry
                .path()
                .extension()
                .and_then(|x| x.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if !matches!(ext.as_str(), "jpg" | "jpeg" | "rw2") {
                continue;
            }
            seen += 1;
            if ext == "rw2" {
                rw2 += 1;
            }
            if read_gps(entry.path()).is_some() {
                if ext == "rw2" {
                    rw2_gps += 1;
                } else {
                    jpg_gps += 1;
                }
            }
            if seen >= 1500 {
                break;
            }
        }
        eprintln!("[gps fixture] scanned={seen} rw2={rw2} jpg_gps={jpg_gps} rw2_gps={rw2_gps}");
    }
}
