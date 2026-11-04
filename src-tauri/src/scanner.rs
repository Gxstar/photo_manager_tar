use crate::error::{AppError, AppResult};
use crate::meta::classify::{self, DirFileMeta, is_ignored_name};
use crate::meta::{photo::photo_shoot_time, video::video_shoot_time};
use crate::models::{AppConfig, CopyPlan, FileKind, FileStatus, MediaFile, TimeSource};
use chrono::{Datelike, NaiveDateTime};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// 遍历时整支剪掉的目录（不进入）：系统目录 + 相机私有目录（内含固件/更新器文件，非用户媒体）
const SKIP_DIRS: &[&str] = &[
    "$recycle.bin",
    "system volume information",
    "private",
    "appinfo",
    "m4hd",
];

fn is_skip_dir(name: &str) -> bool {
    let lower = name.to_lowercase();
    SKIP_DIRS.iter().any(|n| lower == *n)
}

/// 扫描卡片 → 生成拷贝计划（无进度回调，仅测试使用；生产路径走 with_progress）
#[cfg_attr(not(test), allow(dead_code))]
pub fn scan_card(device_path: &str, cfg: &AppConfig) -> AppResult<CopyPlan> {
    scan_card_inner(device_path, cfg, &|_, _, _| {})
}

/// 带进度回调的扫描（cb(done, total, current_file_name)）
pub fn scan_card_with_progress(
    device_path: &str,
    cfg: &AppConfig,
    cb: &dyn Fn(usize, usize, &str),
) -> AppResult<CopyPlan> {
    scan_card_inner(device_path, cfg, cb)
}

/// 扫描实现。
///
/// 去重 = 目标路径存在性检测（每文件一次 stat，同时拿存在性 + 字节数）：
/// - 目标不存在 → New
/// - 存在且同大小 → Exists（跳过）
/// - 存在但不同大小 → Conflict（多半是崩溃残留，报告列出两侧大小）
fn scan_card_inner(
    device_path: &str,
    cfg: &AppConfig,
    cb: &dyn Fn(usize, usize, &str),
) -> AppResult<CopyPlan> {
    let root = PathBuf::from(device_path);
    if !root.is_dir() {
        return Err(AppError::DeviceDisconnected);
    }

    // ── 第一遍：收集卡上全部文件，建立 per-dir 文件表（供 sidecar 关联）──
    struct RawFile {
        path: PathBuf,
        name: String,
        ext: String,
        size: u64,
        kind: FileKind,
    }

    let mut raws: Vec<RawFile> = Vec::new();
    let mut dir_map: HashMap<PathBuf, HashMap<String, DirFileMeta>> = HashMap::new();

    for entry in WalkDir::new(&root)
        .into_iter()
        .filter_entry(|e| !e.file_type().is_dir() || !is_skip_dir(&e.file_name().to_string_lossy()))
    {
        let entry = entry.map_err(|e| AppError::IoError(format!("遍历卡片失败: {e}")))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if is_ignored_name(&name) {
            continue;
        }
        let ext = classify::ext_of(entry.path());
        let kind = classify::classify(&ext);
        let size = entry.metadata().map_err(|e| AppError::IoError(e.to_string()))?.len();

        let parent = entry.path().parent().unwrap_or(Path::new("")).to_path_buf();
        dir_map.entry(parent).or_default().insert(
            name.to_lowercase(),
            DirFileMeta {
                file_name: name.clone(),
                kind,
            },
        );

        raws.push(RawFile {
            path: entry.path().to_path_buf(),
            name,
            ext,
            size,
            kind,
        });
    }

    // ── 第二遍：时间解析 + 目标路径 + 三态判定（逐文件上报进度）──
    let lib = PathBuf::from(&cfg.library_root);
    // 全库字节数索引（只 stat 不读内容，实测全库 <100ms；用于「改名跳过」签名判定）
    let lib_index = build_library_index(&lib, cfg).unwrap_or_default();
    let mut files: Vec<MediaFile> = Vec::new();
    let total = raws.len();
    let mut last_report = std::time::Instant::now();

    for (idx, r) in raws.iter().enumerate() {
        let parent = r.path.parent().unwrap_or(Path::new(""));

        if r.kind == FileKind::Sidecar {
            // sidecar：找主文件，同目录同月，时间继承主文件
            let main = dir_map
                .get(parent)
                .and_then(|m| classify::find_main_file(m, &r.name));

            let (target, month_group, shoot_time, time_source, note) = match main {
                Some(m) => {
                    let main_path = parent.join(&m.file_name);
                    match resolve_time(&main_path, m.kind) {
                        Some((dt, src)) => {
                            let (y, mo) = (dt.year(), dt.month());
                            let target =
                                target_dir(&lib, cfg, m.kind, y, mo).join(&r.name);
                            (
                                target,
                                format!("{y}-{mo:02}"),
                                Some(dt.format("%Y-%m-%d %H:%M:%S").to_string()),
                                src,
                                None,
                            )
                        }
                        None => (
                            pending_dir(&lib, cfg).join(&r.name),
                            "待定".to_string(),
                            None,
                            TimeSource::None,
                            Some("主文件无法确定拍摄时间".to_string()),
                        ),
                    }
                }
                None => (
                    pending_dir(&lib, cfg).join(&r.name),
                    "待定".to_string(),
                    None,
                    TimeSource::None,
                    Some("无主文件".to_string()),
                ),
            };
            let status = stat_target(&target, r.size);
            files.push(MediaFile {
                source_path: r.path.clone(),
                file_name: r.name.clone(),
                ext: r.ext.clone(),
                size: r.size,
                kind: r.kind,
                shoot_time,
                time_source,
                note,
                target_path: target,
                month_group,
                status,
            });
            continue;
        }

        // 普通媒体文件
        let (target, month_group, shoot_time, time_source, note) =
            match resolve_time(&r.path, r.kind) {
                Some((dt, src)) => {
                    let (y, mo) = (dt.year(), dt.month());
                    let target = target_dir(&lib, cfg, r.kind, y, mo).join(&r.name);
                    (
                        target,
                        format!("{y}-{mo:02}"),
                        Some(dt.format("%Y-%m-%d %H:%M:%S").to_string()),
                        src,
                        None,
                    )
                }
                None => (
                    pending_dir(&lib, cfg).join(&r.name),
                    "待定".to_string(),
                    None,
                    TimeSource::None,
                    Some("无法确定拍摄时间".to_string()),
                ),
            };
        let mut status = stat_target(&target, r.size);
        let mut note = note;
        // 改名检测：目标不存在，但库内有同字节数同扩展名、且 EXIF 拍摄秒一致的文件
        if status == FileStatus::New {
            if let Some(lib_path) = detect_renamed(
                &lib_index,
                shoot_time.as_deref(),
                time_source,
                r.size,
                &r.ext,
                &target,
            ) {
                status = FileStatus::SuspectSkipped;
                note = Some(format!("内容签名相同，库内已有：{}", lib_path.display()));
            }
        }
        files.push(MediaFile {
            source_path: r.path.clone(),
            file_name: r.name.clone(),
            ext: r.ext.clone(),
            size: r.size,
            kind: r.kind,
            shoot_time,
            time_source,
            note,
            target_path: target,
            month_group,
            status,
        });

        // 进度上报（80ms 节流；最后一个必报）
        let now = std::time::Instant::now();
        if now.duration_since(last_report) >= std::time::Duration::from_millis(80)
            || idx + 1 == total
        {
            last_report = now;
            cb(idx + 1, total, &r.name);
        }
    }

    // 稳定排序：月份 → 文件名
    files.sort_by(|a, b| {
        a.month_group
            .cmp(&b.month_group)
            .then_with(|| a.file_name.to_lowercase().cmp(&b.file_name.to_lowercase()))
    });

    let total_files = files.len();
    let new_files = files.iter().filter(|f| f.status == FileStatus::New).count();
    let new_bytes: u64 = files
        .iter()
        .filter(|f| f.status == FileStatus::New)
        .map(|f| f.size)
        .sum();
    let exists_files = files
        .iter()
        .filter(|f| f.status == FileStatus::Exists)
        .count();
    let conflict_files = files
        .iter()
        .filter(|f| f.status == FileStatus::Conflict)
        .count();
    let suspect_files = files
        .iter()
        .filter(|f| f.status == FileStatus::SuspectSkipped)
        .count();

    let free_bytes = free_space_of(&lib).unwrap_or(0);
    let space_ok = free_bytes >= new_bytes;

    Ok(CopyPlan {
        device_path: device_path.to_string(),
        library_root: cfg.library_root.clone(),
        files,
        total_files,
        new_files,
        new_bytes,
        exists_files,
        conflict_files,
        suspect_files,
        free_bytes,
        space_ok,
    })
}

/// 全库字节数索引：key = (字节数, 小写扩展名) → 库内路径列表。
/// 只 stat 不读内容，实测全库 18k 文件 <100ms；进程内使用，不落盘。
type LibIndex = HashMap<(u64, String), Vec<PathBuf>>;

fn build_library_index(lib: &Path, cfg: &AppConfig) -> Option<LibIndex> {
    let mut idx: LibIndex = HashMap::new();
    for top in [&cfg.original_dir, &cfg.video_dir] {
        let root = lib.join(top);
        if !root.is_dir() {
            continue;
        }
        for entry in WalkDir::new(&root).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy();
            if is_ignored_name(&name) {
                continue;
            }
            let ext = classify::ext_of(entry.path());
            if ext.is_empty() {
                continue;
            }
            let Ok(m) = entry.metadata() else { continue };
            idx.entry((m.len(), ext)).or_default().push(entry.path().to_path_buf());
        }
    }
    Some(idx)
}

/// 改名检测：库内存在同 (字节数, 扩展名) 且 EXIF 拍摄秒一致的文件 → 判定为改名副本。
///
/// 前提：卡上与库内文件都持有相机写入的权威时间（EXIF / mvhd）；
/// 文件系统时间在拷贝时会变化，不可作签名。签名碰撞（同秒+同字节的不同照片）实际不存在，
/// 万一误判也有报告明细可核对。库侧只读头部 128KB（HDD ~1ms/个），不碰 SD 卡。
fn detect_renamed(
    index: &LibIndex,
    card_shoot_time: Option<&str>,
    card_time_src: TimeSource,
    size: u64,
    ext: &str,
    target: &Path,
) -> Option<PathBuf> {
    // 卡上时间必须是权威来源（EXIF / mvhd），文件名/文件系统时间不可作签名
    if !matches!(
        card_time_src,
        TimeSource::ExifDateTimeOriginal | TimeSource::ExifDateTimeDigitized
    ) {
        return None;
    }
    let card_dt =
        NaiveDateTime::parse_from_str(card_shoot_time?, "%Y-%m-%d %H:%M:%S").ok()?;

    let candidates = index.get(&(size, ext.to_lowercase()))?;
    let kind = classify::classify(ext);
    for lib_path in candidates {
        if lib_path == target {
            continue;
        }
        // 库侧时间：照片走 EXIF（含 RW2 内嵌），视频走 mvhd——解析成功即代表权威时间
        let lib_time = match kind {
            FileKind::Video => video_shoot_time(lib_path),
            FileKind::Image | FileKind::Raw => {
                photo_shoot_time(lib_path).and_then(|r| r.ok()).map(|(t, _)| t)
            }
            _ => None,
        };
        if let Some(lib_dt) = lib_time {
            if lib_dt == card_dt {
                return Some(lib_path.clone());
            }
        }
    }
    None
}

/// 拍摄时间解析（按可信度降级）：
/// EXIF(含 RW2 内嵌) → mvhd → 文件名 IMG_全日期 → 文件创建时间 → 文件名 YYYYMMDD_ → 文件修改时间
fn resolve_time(path: &Path, kind: FileKind) -> Option<(NaiveDateTime, TimeSource)> {
    if matches!(kind, FileKind::Image | FileKind::Raw) {
        if let Some(Ok((t, _))) = photo_shoot_time(path) {
            return Some((t, TimeSource::ExifDateTimeOriginal));
        }
    }
    if kind == FileKind::Video {
        if let Some(t) = video_shoot_time(path) {
            return Some((t, TimeSource::ExifDateTimeOriginal));
        }
    }
    let name = path.file_name().map(|s| s.to_string_lossy().to_string())?;
    if let Some(t) = filename_full_time(&name) {
        return Some((t, TimeSource::FileNameFull));
    }
    let meta = fs::metadata(path).ok();
    if let Some(m) = &meta {
        if let Ok(t) = m.created() {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            return Some((dt.naive_local(), TimeSource::FileSystemCreated));
        }
    }
    if let Some(t) = filename_date_time(&name) {
        return Some((t, TimeSource::FileNameDate));
    }
    if let Some(m) = &meta {
        if let Ok(t) = m.modified() {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            return Some((dt.naive_local(), TimeSource::FileSystemModified));
        }
    }
    None
}

/// `IMG_20260329_123640.dng` → 2026-03-29 12:36:40（全零拒绝）
pub(crate) fn filename_full_time(name: &str) -> Option<NaiveDateTime> {
    let rest = name.strip_prefix("IMG_")?;
    let b = rest.as_bytes();
    if b.len() < 15 || b[8] != b'_' {
        return None;
    }
    let date = &rest[..8];
    let time = &rest[9..15];
    if !date.bytes().all(|c| c.is_ascii_digit()) || !time.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if date.bytes().all(|c| c == b'0') {
        return None;
    }
    NaiveDateTime::parse_from_str(&format!("{date} {time}"), "%Y%m%d %H%M%S").ok()
}

/// `20260304_DC-S9_140.avif` → 2026-03-04 00:00:00（全零拒绝）
pub(crate) fn filename_date_time(name: &str) -> Option<NaiveDateTime> {
    let b = name.as_bytes();
    if b.len() < 9 || !b[..8].iter().all(|c| c.is_ascii_digit()) || b[8] != b'_' {
        return None;
    }
    let date = &name[..8];
    if date.bytes().all(|c| c == b'0') {
        return None;
    }
    chrono::NaiveDate::parse_from_str(date, "%Y%m%d")
        .ok()?
        .and_hms_opt(0, 0, 0)
}

/// 年月目录：{库根}\{原图|视频}\{YYYY}\{YYYY-MM}
fn target_dir(lib: &Path, cfg: &AppConfig, kind: FileKind, year: i32, month: u32) -> PathBuf {
    let top = match kind {
        FileKind::Video => &cfg.video_dir,
        _ => &cfg.original_dir,
    };
    lib.join(top)
        .join(year.to_string())
        .join(format!("{year}-{month:02}"))
}

/// 待定目录：{库根}\{原图}\{待定}（库中已存在的空目录）
fn pending_dir(lib: &Path, cfg: &AppConfig) -> PathBuf {
    lib.join(&cfg.original_dir).join(&cfg.pending_dir)
}

/// 目标三态判定：一次 stat 拿存在性 + 字节数
fn stat_target(target: &Path, size: u64) -> FileStatus {
    match fs::metadata(target) {
        Err(_) => FileStatus::New,
        Ok(m) if m.len() == size => FileStatus::Exists,
        Ok(_) => FileStatus::Conflict,
    }
}

/// 目标盘剩余空间（取库根所在盘符）
fn free_space_of(lib: &Path) -> Option<u64> {
    let s = lib.to_string_lossy();
    if s.len() < 3 || s.as_bytes()[1] != b':' {
        return None;
    }
    crate::device::free_space_of_drive(&format!("{}\\", &s[..2]))
}
