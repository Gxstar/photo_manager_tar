//! 媒体库文件系统监听：变更 → 2s 防抖 → 通知前端重拉图库。
//!
//! 底层 notify 封装 Windows 原生 ReadDirectoryChangesW（事件推送，非轮询）。
//! 防抖聚合拷贝一批文件产生的成百个事件；触发后不做后端差量对比——
//! gallery_list 全量 stat 仅 ~100ms 且缓存命中秒回，前端整体替换最简单可靠。

use crate::state::AppState;
use notify_debouncer_full::notify::RecursiveMode;
use notify_debouncer_full::notify::RecommendedWatcher;
use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, RecommendedCache};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

/// 持有 watcher 使其存活；drop 即停止监听
pub struct WatcherHandle {
    _debouncer: Debouncer<RecommendedWatcher, RecommendedCache>,
}

/// 启动（或重启）媒体库监听；库路径不可用时不监听（静默，手动刷新仍可用）
pub fn spawn(app: &AppHandle) {
    let state = app.state::<AppState>();
    let cfg = state.config.lock().unwrap().clone();
    drop(state);

    let lib = PathBuf::from(&cfg.library_root);
    if !lib.is_dir() {
        return;
    }

    let (tx, rx) = mpsc::channel::<DebounceEventResult>();
    let Ok(mut debouncer) = new_debouncer(Duration::from_secs(2), None, move |events| {
        let _ = tx.send(events);
    }) else {
        return;
    };

    let mut watching = false;
    for sub in [&cfg.original_dir, &cfg.video_dir] {
        let dir = lib.join(sub);
        if dir.is_dir() {
            let _ = debouncer.watch(&dir, RecursiveMode::Recursive);
            watching = true;
        }
    }
    if !watching {
        return;
    }

    let app_h = app.clone();
    std::thread::spawn(move || {
        // 收到防抖聚合后的事件批次：过滤纯 .tmp 噪音（打分临时文件）后通知
        while let Ok(Ok(events)) = rx.recv() {
            let meaningful = events.iter().any(|e| {
                e.paths.iter().any(|p| {
                    p.extension()
                        .and_then(|x| x.to_str())
                        .map(|x| !x.eq_ignore_ascii_case("tmp"))
                        .unwrap_or(true)
                })
            });
            if meaningful {
                let _ = app_h.emit("gallery-changed", ());
            }
        }
    });

    // 放入 state 持有（同时顶替旧 watcher，drop 停止旧监听）
    app.state::<AppState>().watcher.lock().unwrap().replace(WatcherHandle {
        _debouncer: debouncer,
    });
    eprintln!("[watch] 已监听媒体库变更：{}", cfg.library_root);
}
