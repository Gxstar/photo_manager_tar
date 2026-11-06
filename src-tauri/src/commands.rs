use crate::config;
use crate::copier::{CopyControl, run_copy};
use crate::device;
use crate::error::{AppError, AppResult};
use crate::gallery::{self, GalleryInfo, GalleryItem, TimesEvent};
use crate::models::{
    AppConfig, CopyEvent, CopyPlan, Device, FileKind, HomeOverview, ImportFailure, ImportLog,
    LibraryStats, RatingsImportReport, ScanEvent, SidecarExportReport,
};
use crate::scanner;
use crate::state::AppState;
use std::path::{Path, PathBuf};
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

// ── 外部编辑器（网格右键「使用 Photoshop 打开」）────────────────────────

/// 自动探测 Photoshop.exe：扫描常见 Adobe 安装目录下 `Adobe Photoshop *\Photoshop.exe`。
/// 目录名逐年变化（2024 / 2025 / CC 2019），按其中的 4 位年份降序取最新版。
/// 刻意不读注册表：那需要新增 crate，或起 `reg query` 子进程闪黑窗；
/// 装在非默认盘 / 绿色版由设置页手动指定兜底。
fn find_photoshop_exe() -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for key in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Ok(v) = std::env::var(key) {
            roots.push(PathBuf::from(v).join("Adobe"));
        }
    }
    if let Ok(v) = std::env::var("LOCALAPPDATA") {
        roots.push(PathBuf::from(v).join("Programs").join("Adobe"));
    }
    // 环境变量缺失（服务/精简环境）时的兜底
    roots.push(PathBuf::from(r"C:\Program Files\Adobe"));
    roots.push(PathBuf::from(r"C:\Program Files (x86)\Adobe"));

    let mut hits: Vec<(u32, PathBuf)> = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.to_ascii_lowercase().starts_with("adobe photoshop") {
                continue;
            }
            let exe = entry.path().join("Photoshop.exe");
            if !exe.is_file() {
                continue;
            }
            let year = name
                .split_whitespace()
                .filter_map(|s| s.parse::<u32>().ok())
                .find(|y| (1990..=2100).contains(y))
                .unwrap_or(0);
            hits.push((year, exe));
        }
    }
    // 年份优先，同年按路径名降序（重复扫描同一目录不影响结果）
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    hits.into_iter().next().map(|(_, p)| p)
}

/// 手动指定的路径优先；无效或为空时退回自动探测（一个坏路径不该让功能彻底不可用）
fn resolve_photoshop(configured: &str) -> Option<PathBuf> {
    let p = configured.trim();
    if !p.is_empty() {
        let exe = PathBuf::from(p);
        if exe.is_file() {
            return Some(exe);
        }
    }
    find_photoshop_exe()
}

/// 自动探测结果，供设置页展示「自动找到」的路径
#[tauri::command]
pub fn detect_photoshop() -> Option<String> {
    find_photoshop_exe().map(|p| p.to_string_lossy().to_string())
}

/// 指定 / 清除外部编辑器路径；空串 = 恢复自动探测
#[tauri::command]
pub fn set_photoshop_path(path: String, state: State<'_, AppState>) -> AppResult<AppConfig> {
    let trimmed = path.trim().to_string();
    if !trimmed.is_empty() {
        let p = Path::new(&trimmed);
        let is_exe = p
            .extension()
            .map(|e| e.eq_ignore_ascii_case("exe"))
            .unwrap_or(false);
        if !p.is_file() || !is_exe {
            return Err(AppError::IoError("请选择有效的 Photoshop.exe".into()));
        }
    }
    let mut cfg = state.config.lock().unwrap().clone();
    cfg.photoshop_path = trimmed;
    config::save(&cfg)?;
    state.update_config(cfg.clone());
    Ok(cfg)
}

/// 用 Photoshop 打开（可一次传多个文件，PS 命令行原生支持）。
/// 只启动外部程序，绝不改动文件；spawn 后立刻返回，不等待 PS 退出。
#[tauri::command]
pub fn open_with_photoshop(paths: Vec<String>, state: State<'_, AppState>) -> AppResult<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let exe = {
        let cfg = state.config.lock().unwrap();
        resolve_photoshop(&cfg.photoshop_path)
    }
    .ok_or_else(|| {
        AppError::IoError("未找到 Photoshop.exe，请到「设置 → 外部编辑器」手动指定".into())
    })?;

    let files: Vec<&str> = paths
        .iter()
        .map(String::as_str)
        .filter(|p| Path::new(*p).is_file())
        .collect();
    if files.is_empty() {
        return Err(AppError::IoError("文件不存在或已被移动".into()));
    }
    std::process::Command::new(&exe)
        .args(files)
        .spawn()
        .map_err(|e| AppError::IoError(format!("启动 Photoshop 失败：{e}")))?;
    Ok(())
}

// ── 图库 ─────────────────────────────────────────────────────────────────

/// 图库列表：只 stat 不读内容；评分直接从 `ratings` 表（权威源）读，
/// 拍摄时间命中进程内缓存，缺失的交给后台任务补齐（gallery-times 批量推送）。
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

    // 评分与收藏都来自 SQLite（权威源），与文件格式无关
    {
        let ratings = state.meta.all_ratings();
        let favorites = state.meta.all_favorites();
        for it in items.iter_mut() {
            it.rating = ratings.get(&it.path).copied();
            it.favorite = favorites.contains(&it.path);
        }
    }

    let mut pending: Vec<gallery::PendingItem> = Vec::new();
    {
        let times = state.time_cache.lock().unwrap();
        for it in items.iter_mut() {
            let time_known = match times.get(&it.path) {
                Some((m, t)) if *m == it.mtime_secs => {
                    it.shoot_time = t.clone();
                    true
                }
                _ => false,
            };
            if !time_known {
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
            // SQLite 时间缓存回写（降级模式 no-op）
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

/// 批量打分 / 清除评分：**只写 SQLite `ratings` 表，绝不改图片文件**。
/// rating=None 或 0 = 清除（删行）；DB 不可用 / 写失败一律报错（硬规则 #14），
/// 不允许降级静默成功——那等于悄悄丢分。
#[tauri::command]
pub fn gallery_set_rating(
    paths: Vec<String>,
    rating: Option<u8>,
    state: State<'_, AppState>,
) -> AppResult<usize> {
    if let Some(v) = rating {
        if v > 5 {
            return Err(AppError::IoError("评分超出 1-5 范围".into()));
        }
    }
    if !state.meta.is_available() {
        return Err(AppError::IoError("评分数据库不可用，评分未保存".into()));
    }
    let value = rating.filter(|v| *v > 0);
    state.meta.set_ratings_batch(&paths, value).map_err(AppError::IoError)
}

/// 批量收藏 / 取消收藏：**只写 SQLite `favorites` 表，绝不改图片文件**。
/// DB 不可用或写失败一律报错（硬规则 #14），不允许降级静默成功。
#[tauri::command]
pub fn gallery_set_favorite(
    paths: Vec<String>,
    favorite: bool,
    state: State<'_, AppState>,
) -> AppResult<usize> {
    if !state.meta.is_available() {
        return Err(AppError::IoError("评分数据库不可用，收藏未保存".into()));
    }
    state
        .meta
        .set_favorites_batch(&paths, favorite)
        .map_err(AppError::IoError)
}

/// 设置页展示：评分数据库文件路径（备份要复制的就是它）
#[tauri::command]
pub fn ratings_db_path(state: State<'_, AppState>) -> String {
    state.meta.db_location()
}

/// 一次性迁移：从文件内嵌 XMP 读回旧评分（**只读文件**，只插不覆盖 DB 已有值）。
/// 手动触发并回报条数，不猜真库现状。
#[tauri::command]
pub async fn ratings_import_from_files(
    state: State<'_, AppState>,
) -> AppResult<RatingsImportReport> {
    let cfg = state.config.lock().unwrap().clone();
    let items = tauri::async_runtime::spawn_blocking(move || gallery::list_items(&cfg))
        .await
        .map_err(|e| AppError::IoError(e.to_string()))??;
    let existing = state.meta.all_ratings();
    let (entries, mut report) = tauri::async_runtime::spawn_blocking(move || {
        crate::meta::sidecar::import_from_files(&items, &existing)
    })
    .await
    .map_err(|e| AppError::IoError(e.to_string()))?;
    report.imported = state
        .meta
        .insert_ratings(&entries)
        .map_err(AppError::IoError)?;
    Ok(report)
}

/// 手动互操作：把评分导出成 XMP sidecar（默认不执行；绝不覆盖第三方 sidecar）
#[tauri::command]
pub async fn ratings_export_sidecars(
    state: State<'_, AppState>,
) -> AppResult<SidecarExportReport> {
    let ratings = state.meta.all_ratings();
    tauri::async_runtime::spawn_blocking(move || crate::meta::sidecar::export(&ratings))
        .await
        .map_err(|e| AppError::IoError(e.to_string()))
}
