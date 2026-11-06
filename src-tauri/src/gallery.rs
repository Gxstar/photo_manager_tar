//! 图库模块：媒体库文件列表 + 拍摄时间后台补齐 + 单文件信息。
//!
//! 设计要点（docs/开发方案.md §8、§10）：
//! - 列表只 stat 不读内容；拍摄时间在后台线程逐个读（HDD 顺序友好），
//!   每 200 个经 `gallery-times` 事件推送补丁，全部完成后前端一次性重排。
//! - 进程内 `time_cache`（path → (mtime, time)），二次进页秒排。
//! - **评分不在这里**：2026-11-05 起 `ratings` SQLite 表是唯一权威源，
//!   `gallery_list` 直接读取；本模块不再读写文件内嵌 XMP 评分。

use crate::error::{AppError, AppResult};
use crate::meta::classify;
use crate::meta::photo::photo_exif;
use crate::meta::video::{video_dimensions, video_shoot_time};
use crate::models::{AppConfig, FileKind};
use chrono::NaiveDateTime;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Instant, UNIX_EPOCH};
use walkdir::WalkDir;

/// 图库网格单文件（列表 DTO，字段刻意最小化）
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GalleryItem {
    pub path: String,
    pub name: String,
    pub ext: String,
    pub kind: FileKind,
    pub size: u64,
    pub mtime_secs: i64,
    pub shoot_time: Option<String>,
    /// 评分 1-5（来自 `ratings` 表；None = 未评分）。与文件格式无关
    pub rating: Option<u8>,
    /// 收藏（来自 `favorites` 表）；二值、与评分互不派生
    pub favorite: bool,
}

/// `gallery-times` 事件的单文件补丁（只补拍摄时间）
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimePatch {
    pub path: String,
    pub mtime_secs: i64,
    pub shoot_time: Option<String>,
}

/// 时间补齐进度事件（done == total 表示完成，前端收到后一次性重排）
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimesEvent {
    pub done: usize,
    pub total: usize,
    pub patches: Vec<TimePatch>,
}

/// 右侧信息栏内容（选中单个文件时按需读取）
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GalleryInfo {
    pub shoot_time: Option<String>,
    pub make: Option<String>,
    pub model: Option<String>,
    /// 镜头型号
    pub lens: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// 光圈值 f/N
    pub f_number: Option<f64>,
    /// 曝光时间（分子/分母），前端按 num<den 显示 1/N
    pub exposure: Option<ExposureTime>,
    pub iso: Option<u32>,
    /// 实际焦距 mm
    pub focal_length: Option<f64>,
    /// 等效 35mm 焦距 mm（缺省时前端不显示）
    pub focal_length_35mm: Option<u32>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExposureTime {
    pub num: u32,
    pub den: u32,
}

/// 时间缓存：path → (mtime, shoot_time)，供 gallery_list 命中复用
pub type TimeCache = HashMap<String, (i64, Option<String>)>;

/// 后台补齐队列项：cached_time 命中缓存时免于重读
pub struct PendingItem {
    pub path: String,
    pub mtime_secs: i64,
    pub kind: FileKind,
    pub cached_time: Option<String>,
}

const TIME_BATCH: usize = 200;
/// 补齐并发线程数（HDD 多流顺序读，实测吞吐随线程数提升）
const META_WORKERS: usize = 4;
/// 文件头扫描长度：EXIF 在头部，一次读盘即可解析拍摄时间
const HEAD_SCAN: usize = 128 * 1024;

/// 扫描媒体库（原图 + 视频两棵子树），只收 Image/Raw/Video。
/// 只 stat 不读内容：18k 文件量级 <300ms。
pub fn list_items(cfg: &AppConfig) -> AppResult<Vec<GalleryItem>> {
    let lib = Path::new(&cfg.library_root);
    if !lib.is_dir() {
        return Err(AppError::LibraryUnreachable(cfg.library_root.clone()));
    }
    let mut items = Vec::new();
    for top in [&cfg.original_dir, &cfg.video_dir] {
        let root = lib.join(top);
        if !root.is_dir() {
            continue;
        }
        for entry in WalkDir::new(&root).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if classify::is_ignored_name(&name) {
                continue;
            }
            let ext = classify::ext_of(entry.path());
            let kind = classify::classify(&ext);
            if !matches!(kind, FileKind::Image | FileKind::Raw | FileKind::Video) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            items.push(GalleryItem {
                path: entry.path().to_string_lossy().to_string(),
                name,
                ext,
                kind,
                size: meta.len(),
                mtime_secs: mtime_secs(&meta),
                shoot_time: None,
                rating: None,
                favorite: false,
            });
        }
    }
    Ok(items)
}

fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// pub 供 home.rs 的最近导入列表复用（同一套 mtime 口径）
pub fn mtime_of(meta: &std::fs::Metadata) -> i64 {
    mtime_secs(meta)
}

/// 后台补齐拍摄时间：4 线程原子领任务，每凑满 TIME_BATCH 回调一次
/// （乱序完成，done 以实际完成数计）。should_stop 在每个任务前检查——
/// 用户重新进入图库页时旧任务让位。
pub fn fill_meta(
    pending: Vec<PendingItem>,
    on_batch: &(dyn Fn(usize, usize, Vec<TimePatch>) + Send + Sync),
    should_stop: &(dyn Fn() -> bool + Send + Sync),
) {
    let total = pending.len();
    if total == 0 {
        return;
    }
    let started = Instant::now();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let done = std::sync::atomic::AtomicUsize::new(0);
    let buffer: Mutex<Vec<TimePatch>> = Mutex::new(Vec::with_capacity(TIME_BATCH));

    std::thread::scope(|s| {
        for _ in 0..META_WORKERS.min(total) {
            s.spawn(|| loop {
                if should_stop() {
                    return;
                }
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if i >= total {
                    return;
                }
                let patch = parse_meta(&pending[i]);
                let mut buf = buffer.lock().unwrap();
                buf.push(patch);
                if buf.len() >= TIME_BATCH {
                    let take = std::mem::take(&mut *buf);
                    let d = done.fetch_add(take.len(), std::sync::atomic::Ordering::Relaxed)
                        + take.len();
                    on_batch(d, total, take);
                }
            });
        }
    });

    // 尾批（任务被停止时丢弃，与旧语义一致）
    if !should_stop() {
        let rest = std::mem::take(&mut *buffer.lock().unwrap());
        if !rest.is_empty() {
            let d =
                done.fetch_add(rest.len(), std::sync::atomic::Ordering::Relaxed) + rest.len();
            on_batch(d, total, rest);
        }
    }
    eprintln!(
        "[gallery] 拍摄时间补齐完成 {total} 项（{META_WORKERS} 线程），耗时 {:.1}s",
        started.elapsed().as_secs_f64()
    );
}

/// 单文件拍摄时间解析。JPEG/RAW 一次读头部 128KB（EXIF 在头），
/// 失败再回退全文件；ISO BMFF（AVIF/HEIC）与视频走全文件路径。
fn parse_meta(p: &PendingItem) -> TimePatch {
    let path = Path::new(&p.path);
    let mut shoot_time = p.cached_time.clone();

    if shoot_time.is_none() {
        shoot_time = match p.kind {
            FileKind::Video => read_time(path, p.kind),
            FileKind::Image | FileKind::Raw => {
                if crate::meta::bmff::is_bmff(path) {
                    // BMFF 的 EXIF item 在 mdat 之后，头部扫描无意义
                    read_time(path, p.kind)
                } else {
                    let head = read_head(path);
                    crate::meta::photo::photo_time_from_head(&head)
                        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
                        // 头部解析失败 fallback 全文件（个别 EXIF 链超出头部范围）
                        .or_else(|| read_time(path, p.kind))
                }
            }
            _ => None,
        };
    }

    TimePatch {
        path: p.path.clone(),
        mtime_secs: p.mtime_secs,
        shoot_time,
    }
}

fn read_head(path: &Path) -> Vec<u8> {
    let mut buf = vec![0u8; HEAD_SCAN];
    let n = std::fs::File::open(path)
        .and_then(|mut f| std::io::Read::read(&mut f, &mut buf))
        .unwrap_or(0);
    buf.truncate(n);
    buf
}

/// 单文件拍摄时间（照片 EXIF / RW2 内嵌 / 视频 mvhd），格式化 "YYYY-MM-DD HH:MM:SS"
pub fn read_time(path: &Path, kind: FileKind) -> Option<String> {
    let dt: NaiveDateTime = match kind {
        FileKind::Video => video_shoot_time(path)?,
        FileKind::Image | FileKind::Raw => photo_exif(path).and_then(|e| e.time)?,
        _ => return None,
    };
    Some(dt.format("%Y-%m-%d %H:%M:%S").to_string())
}

/// 单文件信息（信息栏按需读取，单文件毫秒级）。评分不在这里，前端用列表 DTO 的值。
pub fn info(path: &str, kind: FileKind) -> GalleryInfo {
    let p = PathBuf::from(path);
    match kind {
        FileKind::Video => {
            let (w, h) = video_dimensions(&p).unwrap_or((0, 0));
            GalleryInfo {
                shoot_time: video_shoot_time(&p)
                    .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string()),
                width: (w > 0).then_some(w),
                height: (h > 0).then_some(h),
                ..GalleryInfo::empty()
            }
        }
        FileKind::Image | FileKind::Raw => match photo_exif(&p) {
            Some(e) => GalleryInfo {
                shoot_time: e.time.map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string()),
                make: e.make,
                model: e.model,
                lens: e.lens,
                width: e.width,
                height: e.height,
                f_number: e.f_number,
                exposure: e.exposure.map(|(num, den)| ExposureTime { num, den }),
                iso: e.iso,
                focal_length: e.focal_length,
                focal_length_35mm: e.focal_length_35mm,
            },
            None => GalleryInfo::empty(),
        },
        _ => GalleryInfo::empty(),
    }
}

impl GalleryInfo {
    fn empty() -> Self {
        Self {
            shoot_time: None,
            make: None,
            model: None,
            lens: None,
            width: None,
            height: None,
            f_number: None,
            exposure: None,
            iso: None,
            focal_length: None,
            focal_length_35mm: None,
        }
    }
}
