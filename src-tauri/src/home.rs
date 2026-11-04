//! 首页空态数据：只枚举最近的月份目录，不做全库遍历。
//!
//! 两个关键点：
//! 1. 库目录结构是 `原图|视频 \ YYYY \ YYYY-MM`，月份目录名本身就是索引。
//!    取最近 N 个月只需读 3 层目录，不必 walkdir 全库（18k 文件 ~ 全量 stat）。
//! 2. 「最近导入」按 mtime 降序。copier 用 `create_new` 直写且**不写 filetime**，
//!    所以库内文件的 mtime 就是导入时间——比 EXIF 拍摄时间更贴合「最近导入」语义，
//!    且零解析成本、不依赖图库的 `fill_meta` 补齐。

use crate::error::{AppError, AppResult};
use crate::meta::classify;
use crate::models::{AppConfig, FileKind, HomeOverview, MonthStat, RecentItem};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 展示的月份 chip 数
const SHOW_MONTHS: usize = 3;
/// 最近导入缩略图数量
const RECENT_N: usize = 12;
/// JPEG 不足这个数才用 RAW/视频补位（RAW 无系统编解码器时 GetImage 会失败）
const MIN_JPEG: usize = 6;
/// 凑不满 RECENT_N 时最多往前翻的月份目录数
const MAX_SCAN_MONTHS: usize = 12;

type Entry = (PathBuf, String, FileKind, u64, i64);

pub fn overview(cfg: &AppConfig) -> AppResult<HomeOverview> {
    let lib = Path::new(&cfg.library_root);
    if !lib.is_dir() {
        return Err(AppError::LibraryUnreachable(cfg.library_root.clone()));
    }

    let groups = month_groups(lib, &cfg.original_dir, &cfg.video_dir);

    // 逐月读取（同月的 原图/视频 合并），读够 SHOW_MONTHS 个月且 JPEG 足够即停
    let mut loaded: Vec<(String, Vec<PathBuf>, Vec<Entry>)> = Vec::new();
    let mut jpegs = 0usize;
    for (label, dirs) in groups.iter().take(MAX_SCAN_MONTHS) {
        let mut files: Vec<Entry> = Vec::new();
        for dir in dirs {
            files.extend(media_files(dir));
        }
        jpegs += files
            .iter()
            .filter(|f| matches!(f.2, FileKind::Image))
            .count();
        loaded.push((label.clone(), dirs.clone(), files));
        if loaded.len() >= SHOW_MONTHS && jpegs >= RECENT_N {
            break;
        }
    }

    // 月份统计（只算媒体文件，与图库口径一致；sidecar 不计）
    let mut months: Vec<MonthStat> = loaded
        .iter()
        .take(SHOW_MONTHS)
        .map(|(label, dirs, files)| MonthStat {
            label: label.clone(),
            dir: dirs[0].to_string_lossy().to_string(),
            files: files.len(),
            bytes: files.iter().map(|f| f.3).sum(),
        })
        .collect();
    months.retain(|m| m.files > 0);

    // 最近导入：JPEG 优先（保证出图率），不足 MIN_JPEG 才用 RAW/视频补位
    let mut cands: Vec<RecentItem> = loaded
        .iter()
        .flat_map(|(_, _, files)| files.iter())
        .map(|(path, name, kind, _, mtime)| RecentItem {
            path: path.to_string_lossy().to_string(),
            name: name.clone(),
            kind: *kind,
            mtime_secs: *mtime,
        })
        .collect();
    cands.sort_by(|a, b| b.mtime_secs.cmp(&a.mtime_secs));

    let mut recent: Vec<RecentItem> = cands
        .iter()
        .filter(|c| matches!(c.kind, FileKind::Image))
        .take(RECENT_N)
        .cloned()
        .collect();
    if recent.len() < MIN_JPEG {
        for c in cands.iter().filter(|c| !matches!(c.kind, FileKind::Image)) {
            if recent.len() >= RECENT_N {
                break;
            }
            recent.push(c.clone());
        }
        recent.sort_by(|a, b| b.mtime_secs.cmp(&a.mtime_secs));
    }

    Ok(HomeOverview {
        months,
        recent,
        imports: Vec::new(), // 由 commands::home_overview 从 SQLite 填充
    })
}

/// 枚举 `原图|视频 \ YYYY \ YYYY-MM`，返回按月份倒序的 (label, 该月所有目录)。
/// 同一个月在「原图」与「视频」下各有一个目录，合并为一组；「原图」排在首位，
/// 作为点击 chip 时打开的路径。
fn month_groups(lib: &Path, original: &str, video: &str) -> Vec<(String, Vec<PathBuf>)> {
    let mut map: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for top in [original, video] {
        let root = lib.join(top);
        let Ok(years) = std::fs::read_dir(&root) else {
            continue;
        };
        for y in years.flatten() {
            if !y.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let Ok(months) = std::fs::read_dir(y.path()) else {
                continue;
            };
            for m in months.flatten() {
                if !m.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let label = m.file_name().to_string_lossy().to_string();
                // 缺月不是错误，非 YYYY-MM 的目录（如「待定」）直接跳过
                if !is_month_label(&label) {
                    continue;
                }
                let entry = map.entry(label).or_default();
                if top == original {
                    entry.insert(0, m.path());
                } else {
                    entry.push(m.path());
                }
            }
        }
    }
    map.into_iter().rev().collect()
}

/// 读一层目录，只留 Image/Raw/Video，跳过系统文件
fn media_files(dir: &Path) -> Vec<Entry> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        if !ft.is_file() {
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        if classify::is_ignored_name(&name) {
            continue;
        }
        let path = e.path();
        let ext = classify::ext_of(&path);
        let kind = classify::classify(&ext);
        if !matches!(kind, FileKind::Image | FileKind::Raw | FileKind::Video) {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        out.push((path, name, kind, meta.len(), crate::gallery::mtime_of(&meta)));
    }
    out
}

fn is_month_label(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 7
        && b[4] == b'-'
        && b[..4].iter().all(|c| c.is_ascii_digit())
        && b[5..].iter().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::{is_month_label, overview, RECENT_N, SHOW_MONTHS};
    use crate::models::AppConfig;

    #[test]
    fn month_label_format() {
        assert!(is_month_label("2026-10"));
        assert!(is_month_label("1999-01"));
        assert!(!is_month_label("2026-1"));
        assert!(!is_month_label("2026-10-01"));
        assert!(!is_month_label("待定"));
        assert!(!is_month_label("2026_10"));
        assert!(!is_month_label("abcd-ef"));
    }

    /// 真实库冒烟（只读）：SDI_FIXTURE_ROOT 指向媒体库根时才跑
    #[test]
    #[ignore]
    fn overview_real_library() {
        let Ok(root) = std::env::var("SDI_FIXTURE_ROOT") else {
            eprintln!("跳过：未配置 SDI_FIXTURE_ROOT");
            return;
        };
        let cfg = AppConfig {
            library_root: root,
            ..Default::default()
        };
        let ov = overview(&cfg).expect("overview 失败");

        assert!(!ov.months.is_empty(), "应至少返回一个月份");
        assert!(ov.months.len() <= SHOW_MONTHS);
        let labels: Vec<&String> = ov.months.iter().map(|m| &m.label).collect();
        let mut sorted = labels.clone();
        sorted.sort();
        sorted.reverse();
        assert_eq!(labels, sorted, "月份必须倒序");

        assert!(ov.recent.len() <= RECENT_N);
        for w in ov.recent.windows(2) {
            assert!(w[0].mtime_secs >= w[1].mtime_secs, "recent 必须按 mtime 降序");
        }
        for r in &ov.recent {
            assert!(std::path::Path::new(&r.path).exists(), "路径不存在: {}", r.path);
        }

        for m in &ov.months {
            eprintln!("月份 {} · {} 个 · {} 字节 · {}", m.label, m.files, m.bytes, m.dir);
        }
        for r in &ov.recent {
            eprintln!("recent [{:?}] {}", r.kind, r.name);
        }
        let jpegs = ov
            .recent
            .iter()
            .filter(|r| matches!(r.kind, crate::models::FileKind::Image))
            .count();
        eprintln!("recent 共 {} 张，其中 JPEG {} 张", ov.recent.len(), jpegs);
    }
}
