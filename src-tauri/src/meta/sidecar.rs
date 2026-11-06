//! XMP sidecar 互操作（docs/开发方案.md §10.4，2026-11-05）。
//!
//! 评分权威源是 SQLite `ratings` 表；sidecar 只是**手动导出**的交换格式，
//! 给 Adobe Bridge / Lightroom / Camera Raw、darktable、XnViewMP 这类 DAM 读。
//! 默认不生成任何 sidecar；**绝不修改图片主文件**。
//!
//! 命名：`<完整文件名>.xmp`（保留原扩展名），与库内既有 darktable sidecar 形态一致，
//! 且不覆盖 Adobe 风格的 `P1161462.xmp`。
//! 安全规则：目标已存在且不含 `develop:source="develop"` → 判为第三方 sidecar，
//! 一律跳过并回报（库里还有带编辑记录的 darktable / DNG sidecar，覆盖不可逆）。

use crate::gallery::GalleryItem;
use crate::models::{RatingsImportReport, SidecarExportReport};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 本工具生成的标识：只有带它的 sidecar 允许被覆盖更新
const OWN_MARK: &[u8] = b"develop:source=\"develop\"";
/// 报告里最多列出多少个跳过 / 不可读文件，避免真库 6k+ 条撑爆 IPC
const REPORT_LIMIT: usize = 20;

/// `P1160211.RW2` → `P1160211.RW2.xmp`
pub fn sidecar_path(main: &Path) -> PathBuf {
    let name = main
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    main.with_file_name(format!("{name}.xmp"))
}

/// 最小可互操作 XMP 包：只写 `xmp:Rating`（属性式）+ 来源标识。
/// **不写 `MicrosoftPhoto:Rating`**：WIC 的 `System.Rating` 值域 0-99 且从文件本体读，
/// 不读 sidecar，写了只会制造新的不一致。
pub(crate) fn packet(rating: u8) -> String {
    format!(
        "<?xpacket begin=\"\u{FEFF}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
         <x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"Develop\">\n \
         <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n  \
         <rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\" \
         xmlns:develop=\"https://gengx.cn/develop/1.0/\" xmp:Rating=\"{rating}\" \
         develop:source=\"develop\"/>\n \
         </rdf:RDF>\n\
         </x:xmpmeta>\n\
         <?xpacket end=\"w\"?>"
    )
}

fn is_ours(content: &[u8]) -> bool {
    content
        .windows(OWN_MARK.len())
        .any(|w| w == OWN_MARK)
}

/// 同目录 tmp + rename 原子写（watcher 已过滤 .tmp 噪音）
fn atomic_write(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!("{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, content)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// 把 `ratings` 表导出为 sidecar。返回新建 / 更新 / 跳过 / 失败明细。
pub fn export(ratings: &HashMap<String, u8>) -> SidecarExportReport {
    let mut report = SidecarExportReport {
        written: 0,
        updated: 0,
        skipped: 0,
        skipped_files: Vec::new(),
        failed: Vec::new(),
    };
    let mut entries: Vec<_> = ratings.iter().collect();
    entries.sort_by_key(|(p, _)| *p); // 报告顺序稳定

    for (path, rating) in entries {
        let main = Path::new(path);
        if !main.is_file() {
            if report.failed.len() < REPORT_LIMIT {
                report.failed.push(format!("{path}（主文件不存在）"));
            }
            continue;
        }
        let sc = sidecar_path(main);
        let existing = std::fs::read(&sc).ok();
        if let Some(old) = &existing {
            if !is_ours(old) {
                report.skipped += 1;
                if report.skipped_files.len() < REPORT_LIMIT {
                    report.skipped_files.push(sc.to_string_lossy().to_string());
                }
                continue; // 第三方 sidecar 绝不覆盖
            }
        }
        match atomic_write(&sc, packet(*rating).as_bytes()) {
            Ok(()) => {
                if existing.is_some() {
                    report.updated += 1;
                } else {
                    report.written += 1;
                }
            }
            Err(e) => {
                if report.failed.len() < REPORT_LIMIT {
                    report.failed.push(format!("{}（{e}）", sc.display()));
                }
            }
        }
    }
    report
}

/// 一次性从**文件内嵌 XMP** 读回旧评分（只读，不写文件）。
/// 只覆盖 JPEG APP1 与 ISO BMFF mime item；厂商 RAW、视频等一律计入 unreadable
/// （11-05 已删除 RAW 解析分支，不为个别旧评分恢复整套写入代码）。
/// 已存在于 `ratings` 表的路径不覆盖——用户在本工具打的分优先。
pub fn import_from_files(
    items: &[GalleryItem],
    existing: &HashMap<String, u8>,
) -> (Vec<(String, u8)>, RatingsImportReport) {
    let mut report = RatingsImportReport {
        imported: 0,
        skipped_existing: 0,
        no_rating: 0,
        unreadable: 0,
        unreadable_files: Vec::new(),
    };
    let mut entries = Vec::new();

    for it in items {
        if existing.contains_key(&it.path) {
            report.skipped_existing += 1;
            continue;
        }
        let p = Path::new(&it.path);
        let ext = it.ext.to_ascii_lowercase();
        let bmff = crate::meta::bmff::is_bmff(p);
        if !bmff && !matches!(ext.as_str(), "jpg" | "jpeg") {
            report.unreadable += 1;
            if report.unreadable_files.len() < REPORT_LIMIT {
                report.unreadable_files.push(it.name.clone());
            }
            continue;
        }
        let r = if bmff {
            crate::meta::bmff::rating(p)
        } else {
            crate::meta::xmp::rating(p)
        };
        match r {
            Some(v) => entries.push((it.path.clone(), v)),
            None => report.no_rating += 1,
        }
    }
    (entries, report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(case: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sdi-sidecar-{case}"));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn packet_is_parseable() {
        assert_eq!(crate::meta::xmp::parse_rating(packet(4).as_bytes()), Some(4));
        assert!(is_ours(packet(2).as_bytes()));
        assert_eq!(sidecar_path(Path::new("D:/lib/P1160211.RW2")),
                   PathBuf::from("D:/lib/P1160211.RW2.xmp"));
    }

    #[test]
    fn export_roundtrip_leaves_main_untouched() {
        let dir = tmp("roundtrip");
        let main = dir.join("a.jpg");
        std::fs::write(&main, b"original-photo-bytes").unwrap();
        let before = std::fs::read(&main).unwrap();
        let mtime = std::fs::metadata(&main).unwrap().modified().unwrap();

        let mut ratings = HashMap::new();
        ratings.insert(main.to_string_lossy().to_string(), 4u8);
        let r1 = export(&ratings);
        assert_eq!((r1.written, r1.updated, r1.skipped), (1, 0, 0));

        let sc = sidecar_path(&main);
        assert_eq!(
            crate::meta::xmp::parse_rating(&std::fs::read(&sc).unwrap()),
            Some(4)
        );
        assert_eq!(std::fs::read(&main).unwrap(), before, "主文件内容零改动");
        assert_eq!(std::fs::metadata(&main).unwrap().modified().unwrap(), mtime, "主文件 mtime 零改动");

        // 再次导出 → 更新本工具自己的 sidecar
        ratings.insert(main.to_string_lossy().to_string(), 2u8);
        let r2 = export(&ratings);
        assert_eq!((r2.written, r2.updated, r2.skipped), (0, 1, 0));
        assert_eq!(
            crate::meta::xmp::parse_rating(&std::fs::read(&sc).unwrap()),
            Some(2)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn export_skips_foreign_sidecar() {
        let dir = tmp("foreign");
        let main = dir.join("b.RW2");
        std::fs::write(&main, b"raw-bytes").unwrap();
        let sc = sidecar_path(&main);
        let foreign = b"<?xpacket begin=\"\"?><x:xmpmeta darktable:history_end=\"12\"/><?xpacket end=\"w\"?>";
        std::fs::write(&sc, foreign).unwrap();

        let mut ratings = HashMap::new();
        ratings.insert(main.to_string_lossy().to_string(), 5u8);
        let r = export(&ratings);
        assert_eq!((r.written, r.updated, r.skipped), (0, 0, 1));
        assert_eq!(r.skipped_files.len(), 1);
        assert_eq!(std::fs::read(&sc).unwrap(), foreign, "第三方 sidecar 分毫不动");
        let _ = std::fs::remove_dir_all(dir);
    }
}
