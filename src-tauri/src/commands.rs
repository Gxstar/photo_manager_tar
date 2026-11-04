use crate::config;
use crate::copier::{CopyControl, run_copy};
use crate::device;
use crate::error::{AppError, AppResult};
use crate::models::{AppConfig, CopyEvent, CopyPlan, Device, LibraryStats, ScanEvent};
use crate::scanner;
use crate::state::AppState;
use std::path::Path;
use std::time::Instant;
use tauri::{AppHandle, Emitter, State, ipc::Channel};

/// 读取配置
#[tauri::command]
pub fn get_config(state: State<'_, AppState>) -> AppConfig {
    state.config.lock().unwrap().clone()
}

/// 修改媒体库根路径（软件内设置），持久化
#[tauri::command]
pub fn set_library_root(root: String, state: State<'_, AppState>) -> AppResult<AppConfig> {
    let trimmed = root.trim().trim_end_matches(['\\', '/']).to_string();
    if trimmed.is_empty() || !Path::new(&trimmed).is_dir() {
        return Err(AppError::LibraryUnreachable(trimmed));
    }
    let mut cfg = state.config.lock().unwrap().clone();
    cfg.library_root = trimmed;
    config::save(&cfg)?;
    state.update_config(cfg.clone());
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

    tauri::async_runtime::spawn_blocking(move || {
        run_copy(plan, control, on_event);
        // 拷贝结束后推送最新库统计
        if let Ok(stats) = library_stats_for(&config::load().library_root) {
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
    let root = state.config.lock().unwrap().library_root.clone();
    library_stats_for(&root)
}

fn library_stats_for(root: &str) -> AppResult<LibraryStats> {
    let lib = Path::new(root);
    if !lib.is_dir() {
        return Err(AppError::LibraryUnreachable(root.to_string()));
    }
    let mut total_files = 0usize;
    let mut total_bytes: u64 = 0;
    for entry in walkdir::WalkDir::new(lib).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if name == "desktop.ini" || name == "thumbs.db" {
                continue;
            }
            total_files += 1;
            if let Ok(m) = entry.metadata() {
                total_bytes += m.len();
            }
        }
    }
    let free = {
        let s = root.to_string();
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
