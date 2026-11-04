use crate::config;
use crate::copier::{CopyControl, run_copy};
use crate::device;
use crate::error::{AppError, AppResult};
use crate::gallery::{self, GalleryInfo, GalleryItem, TimesEvent};
use crate::models::{
    AppConfig, CopyEvent, CopyPlan, Device, FileKind, HomeOverview, ImportFailure, ImportLog,
    LibraryStats, ScanEvent,
};
use crate::scanner;
use crate::state::AppState;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Instant;
use tauri::{AppHandle, Emitter, Manager, State, ipc::Channel};

/// 读取配置
#[tauri::command]
pub fn get_config(state: State<'_, AppState>) -> AppConfig {
    state.config.lock().unwrap().clone()
}

/// 修改媒体库根路径（软件内设置），持久化；同时重启热刷新监听
#[tauri::command]
pub fn set_library_root(
    root: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> AppResult<AppConfig> {
    let trimmed = root.trim().trim_end_matches(['\\', '/']).to_string();
    if trimmed.is_empty() || !Path::new(&trimmed).is_dir() {
        return Err(AppError::LibraryUnreachable(trimmed));
    }
    let mut cfg = state.config.lock().unwrap().clone();
    cfg.library_root = trimmed;
    config::save(&cfg)?;
    state.update_config(cfg.clone());
    // 库根变更：重启热刷新监听
    crate::watch::spawn(&app);
    Ok(cfg)
}

/// 枚举可移动设备 + 相机卡判定
#[tauri::command]
pub fn list_devices() -> Vec<Device> {
    device::list_devices()
}

/// 扫描卡片：后台线程执行（不阻塞 UI），经 Channel 推送进度，
/// 完成时通过 `ScanEvent::Finished` 回传完整计划。
#[tauri::command]
pub async fn scan_card(
    device_path: String,
    on_event: Channel<ScanEvent>,
    state: State<'_, AppState>,
) -> AppResult<()> {
    let cfg = state.config.lock().unwrap().clone();
    if cfg.library_root.is_empty() {
        return Err(AppError::LibraryUnreachable(
            "未设置媒体库路径，请先到设置页配置".into(),
        ));
    }

    tauri::async_runtime::spawn_blocking(move || {
        let started = Instant::now();
        let result = scanner::scan_card_with_progress(&device_path, &cfg, &|done, total, name| {
            let _ = on_event.send(ScanEvent::Progress {
                done,
                total,
                current_file: name.to_string(),
            });
        });

        match result {
            Ok(plan) => {
                eprintln!(
                    "[scan] {} 个文件耗时 {:.1}s",
                    plan.total_files,
                    started.elapsed().as_secs_f64()
                );
                let _ = on_event.send(ScanEvent::Finished { plan });
            }
            Err(e) => {
                let _ = on_event.send(ScanEvent::Failed {
                    message: e.to_string(),
                });
            }
        }
    });
    Ok(())
}

/// 开始拷贝：blocking 线程池执行，经 Channel 推送进度
#[tauri::command]
pub fn start_copy(
    plan: CopyPlan,
    on_event: Channel<CopyEvent>,
    state: State<'_, AppState>,
    app: AppHandle,
) -> AppResult<()> {
    // 同一时间只允许一个拷贝任务
    let control = {
        let mut guard = state.control.lock().unwrap();
        if let Some(c) = guard.as_ref() {
            if !c.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(AppError::TaskBusy);
            }
        }
        let c = CopyControl::new();
        *guard = Some(c.clone());
        c
    };

    let plan_snapshot = plan.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let summary = run_copy(plan, control, on_event);

        // 导入日志：每次拷贝一条（取消也记，标记 cancelled），失败明细逐条落库。
        // 成功文件不必记录——库内文件的 mtime + 归档路径足以还原；失败原因不可恢复，才值得存
        let rec = ImportLog {
            id: 0,
            finished_at: chrono::Local::now()
                .format("%Y-%m-%d %H:%M:%S")
                .to_string(),
            device_path: plan_snapshot.device_path.clone(),
            library_root: plan_snapshot.library_root.clone(),
            files_total: plan_snapshot.total_files,
            files_copied: summary.copied,
            files_skipped: plan_snapshot.exists_files,
            files_conflict: plan_snapshot.conflict_files + plan_snapshot.suspect_files,
            files_failed: summary.failed,
            bytes_copied: summary.bytes_copied,
            elapsed_secs: summary.elapsed_secs,
            avg_bytes_per_sec: summary.avg_bytes_per_sec,
            cancelled: summary.cancelled,
            failures: summary
                .failures
                .iter()
                .map(|(name, reason)| ImportFailure {
                    file_name: name.clone(),
                    reason: reason.clone(),
                })
                .collect(),
        };
        if let Some(st) = app.try_state::<AppState>() {
            st.meta.log_import(&rec);
        }

        // 拷贝结束后推送最新库统计
        if let Ok(stats) = library_stats_for(&config::load()) {
            let _ = app.emit("library-stats", stats);
        }
    });
    Ok(())
}

/// 取消当前拷贝任务
#[tauri::command]
pub fn cancel_copy(state: State<'_, AppState>) -> AppResult<()> {
    let guard = state.control.lock().unwrap();
    match guard.as_ref() {
        Some(c) => {
            c.cancel
                .store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
        None => Err(AppError::NoTask),
    }
}

/// 暂停 / 继续
#[tauri::command]
pub fn pause_copy(state: State<'_, AppState>, paused: bool) -> AppResult<()> {
    let guard = state.control.lock().unwrap();
    match guard.as_ref() {
        Some(c) => {
            c.pause
                .store(paused, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
        None => Err(AppError::NoTask),
    }
}

/// 媒体库统计：文件数 / 总占用 / 剩余空间
#[tauri::command]
pub fn library_stats(state: State<'_, AppState>) -> AppResult<LibraryStats> {
    let cfg = state.config.lock().unwrap().clone();
    library_stats_for(&cfg)
}

/// 口径与图库一致：复用 `gallery::list_items`（原图 + 视频两棵子树，
/// 按 image/raw/video 分类，忽略 desktop.ini 等）。
/// 之前是全库 WalkDir 统计所有文件，侧边栏 18,498 与图库 13,177 并列显示，
/// 看上去像数据坏了——用户要求统一为「图库能显示的总数量」。
pub(crate) fn library_stats_for(cfg: &AppConfig) -> AppResult<LibraryStats> {
    let lib = Path::new(&cfg.library_root);
    if !lib.is_dir() {
        return Err(AppError::LibraryUnreachable(cfg.library_root.clone()));
    }
    let items = gallery::list_items(cfg)?;
    let total_files = items.len();
    let total_bytes: u64 = items.iter().map(|it| it.size).sum();
    let free = {
        let s = cfg.library_root.as_str();
        if s.len() >= 3 && s.as_bytes()[1] == b':' {
            device::free_space_of_drive(&format!("{}\\", &s[..2])).unwrap_or(0)
        } else {
            0
        }
    };
    Ok(LibraryStats {
        total_files,
        total_bytes,
        free_bytes: free,
    })
}

// ── 首页 ─────────────────────────────────────────────────────────────────

/// 首页空态数据（无卡时展示）：最近月份归档统计 + 最近导入缩略图。
/// 只读最近若干个月份目录，不做全库遍历（对照 `library_stats` 的 18k 全量 stat）。
#[tauri::command]
pub async fn home_overview(state: State<'_, AppState>) -> AppResult<HomeOverview> {
    let cfg = state.config.lock().unwrap().clone();
    let mut ov = tauri::async_runtime::spawn_blocking(move || crate::home::overview(&cfg))
        .await
        .map_err(|e| AppError::IoError(e.to_string()))??;
    // 导入日志走 SQLite，与文件扫描解耦（一次 IPC 拿全首页所需数据）
    ov.imports = state.meta.recent_imports(5);
    Ok(ov)
}

/// 清空导入记录，返回删除条数
#[tauri::command]
pub fn import_log_clear(state: State<'_, AppState>) -> usize {
    state.meta.clear_import_log()
}

/// 记录上次停留的页面（启动时恢复；有卡时前端会强制进「拷贝整理」）
#[tauri::command]
pub fn set_last_page(page: String, state: State<'_, AppState>) -> AppResult<()> {
    let mut cfg = state.config.lock().unwrap().clone();
    if cfg.last_page == page {
        return Ok(());
    }
    cfg.last_page = page;
    config::save(&cfg)?;
    state.update_config(cfg);
    Ok(())
}

/// 开关：插卡后自动扫描（只读卡，不改卡、不自动拷贝）
#[tauri::command]
pub fn set_auto_scan(enabled: bool, state: State<'_, AppState>) -> AppResult<AppConfig> {
    let mut cfg = state.config.lock().unwrap().clone();
    if cfg.auto_scan == enabled {
        return Ok(cfg);
    }
    cfg.auto_scan = enabled;
    config::save(&cfg)?;
    state.update_config(cfg.clone());
    Ok(cfg)
}

/// 在资源管理器中打开
#[tauri::command]
pub fn open_in_explorer(path: String) -> AppResult<()> {
    let p = Path::new(&path);
    let target = if p.is_dir() {
        p.to_path_buf()
    } else {
        p.parent().unwrap_or(p).to_path_buf()
    };
    tauri_plugin_opener::open_path(target.to_string_lossy().to_string(), None::<&str>)
        .map_err(|e| AppError::IoError(e.to_string()))?;
    Ok(())
}

// ── 图库 ─────────────────────────────────────────────────────────────────

/// 图库列表：只 stat 不读内容；命中进程内时间/评分缓存；
/// 缺失的元数据交给后台任务逐个补齐（gallery-times 事件批量推送）
#[tauri::command]
pub async fn gallery_list(
    app: AppHandle,
    state: State<'_, AppState>,
) -> AppResult<Vec<GalleryItem>> {
    let cfg = state.config.lock().unwrap().clone();
    let mut items = tauri::async_runtime::spawn_blocking(move || gallery::list_items(&cfg))
        .await
        .map_err(|e| AppError::IoError(e.to_string()))??;

    items.sort_by(|a, b| a.path.to_lowercase().cmp(&b.path.to_lowercase()));

    // 命中缓存 → 二次进页直接带元数据返回
    let mut pending: Vec<gallery::PendingItem> = Vec::new();
    {
        let times = state.time_cache.lock().unwrap();
        let ratings = state.rating_cache.lock().unwrap();
        for it in items.iter_mut() {
            let time_known = match times.get(&it.path) {
                Some((m, t)) if *m == it.mtime_secs => {
                    it.shoot_time = t.clone();
                    true
                }
                _ => false,
            };
            let rating_known = match ratings.get(&it.path) {
                Some((m, r)) if *m == it.mtime_secs => {
                    it.rating = *r;
                    true
                }
                _ => false,
            };
            let need_rating = crate::meta::xmp::is_ratable(Path::new(&it.path));
            if !time_known || (need_rating && !rating_known) {
                pending.push(gallery::PendingItem {
                    path: it.path.clone(),
                    mtime_secs: it.mtime_secs,
                    kind: it.kind,
                    cached_time: it.shoot_time.clone(),
                });
            }
        }
    }

    if !pending.is_empty() {
        let scan_gen = state.gallery_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let app_emit = app.clone();
        let app_stop = app.clone();
        let on_batch = move |done: usize, total: usize, patches: Vec<gallery::TimePatch>| {
            let st = app_emit.state::<AppState>();
            {
                let mut cache = st.time_cache.lock().unwrap();
                for p in &patches {
                    cache.insert(p.path.clone(), (p.mtime_secs, p.shoot_time.clone()));
                }
            }
            {
                let mut cache = st.rating_cache.lock().unwrap();
                for p in &patches {
                    if crate::meta::xmp::is_ratable(Path::new(&p.path)) {
                        cache.insert(p.path.clone(), (p.mtime_secs, p.rating));
                    }
                }
            }
            // SQLite 持久回写（降级模式 no-op）
            st.meta.upsert_patches(&patches);
            let _ = app_emit.emit("gallery-times", TimesEvent { done, total, patches });
        };
        let should_stop = move || {
            app_stop.state::<AppState>().gallery_gen.load(Ordering::SeqCst) != scan_gen
        };
        tauri::async_runtime::spawn_blocking(move || {
            gallery::fill_meta(pending, &on_batch, &should_stop);
        });
    }
    Ok(items)
}

/// 单文件信息（信息栏按需读取）
#[tauri::command]
pub async fn gallery_info(path: String, kind: FileKind) -> AppResult<GalleryInfo> {
    tauri::async_runtime::spawn_blocking(move || Ok(gallery::info(&path, kind)))
        .await
        .map_err(|e| AppError::IoError(e.to_string()))?
}

/// 双击用系统默认程序打开
#[tauri::command]
pub fn gallery_open(path: String) -> AppResult<()> {
    tauri_plugin_opener::open_path(path, None::<&str>)
        .map_err(|e| AppError::IoError(e.to_string()))
}

/// 按容器类型分派写入：
/// - JPEG：XMP 在 APP1 段，长度可能变 → 整文件重写（tmp + rename）
/// - TIFF 容器 RAW（RW2/DNG/…）：XMP 在 IFD 数据区，值恒为单字符
///   → 原地改 1 个字节，文件大小与所有偏移不动
/// - AVIF/HEIF（ISO BMFF）：XMP 在 mime item 的载荷里（文件尾部），同样单字符
///   → 原地改 1 个字节
fn write_rating_for(path: &Path, value: u8) -> Result<(), String> {
    use crate::meta::{bmff, xmp};
    if xmp::is_jpeg(path) {
        xmp::write_jpeg_rating(path, value)
    } else if xmp::is_raw(path) {
        xmp::write_raw_rating(path, value)
    } else if bmff::is_bmff(path) {
        bmff::write_rating(path, value)
    } else {
        Err("该文件类型不支持评分".to_string())
    }
}

/// 批量写入 XMP 评分（JPEG 与 RAW 均写文件内嵌 XMP；rating=None 表示清除）。
/// 返回写入失败的「文件名: 原因」列表（空 = 全部成功）。
#[tauri::command]
pub async fn gallery_set_rating(
    paths: Vec<String>,
    rating: Option<u8>,
    app: AppHandle,
) -> AppResult<Vec<String>> {
    let value = rating.unwrap_or(0);
    if value > 5 {
        return Err(AppError::IoError("评分超出 1-5 范围".into()));
    }
    let results = tauri::async_runtime::spawn_blocking(move || {
        paths
            .into_iter()
            .map(|p| {
                let path = Path::new(&p);
                let r = write_rating_for(path, value).and_then(|_| {
                    std::fs::metadata(path)
                        .map(|m| gallery::mtime_of(&m))
                        .map_err(|e| e.to_string())
                });
                (p, r)
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| AppError::IoError(e.to_string()))?;

    let st = app.state::<AppState>();
    let mut failed = Vec::new();
    {
        let mut cache = st.rating_cache.lock().unwrap();
        for (p, r) in results {
            match r {
                Ok(mtime) => {
                    cache.insert(p.clone(), (mtime, (value > 0).then_some(value)));
                    // SQLite 持久回写
                    st.meta.upsert_rating(&p, mtime, (value > 0).then_some(value));
                }
                Err(e) => {
                    let name = Path::new(&p)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| p.clone());
                    failed.push(format!("{name}: {e}"));
                }
            }
        }
    }
    Ok(failed)
}
