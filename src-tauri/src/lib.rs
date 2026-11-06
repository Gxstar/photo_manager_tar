mod commands;
mod config;
mod copier;
mod device;
mod error;
mod gallery;
mod home;
mod meta;
mod meta_store;
mod models;
mod scanner;
mod state;
mod thumbs;
mod watch;

#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::time::UNIX_EPOCH;
use tauri::http::Response;
use tauri::Manager;

/// thumb:// 协议响应
fn thumb_response(status: u16, body: Vec<u8>) -> Response<Vec<u8>> {
    Response::builder()
        .status(status)
        .header("Content-Type", "image/jpeg")
        // 缓存键已含 mtime+size，文件变更即换键 → 内容可视为永久有效
        .header("Cache-Control", "max-age=31536000, immutable")
        .body(body)
        .unwrap()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(state::AppState::new())
        .setup(|app| {
            // 媒体库热刷新监听：文件增删改 → 防抖 → 前端重拉图库
            watch::spawn(&app.handle());
            Ok(())
        })
        // 缩略图协议：http://thumb.localhost/<percent-encode 的绝对路径>
        .register_asynchronous_uri_scheme_protocol("thumb", |ctx, request, responder| {
            let raw = request.uri().path().trim_start_matches('/').to_string();
            let app = ctx.app_handle().clone();
            tauri::async_runtime::spawn(async move {
                let path = percent_encoding::percent_decode_str(&raw)
                    .decode_utf8_lossy()
                    .to_string();
                let resp = match std::fs::metadata(&path) {
                    Ok(m) => {
                        let mtime = m
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);
                        let size = m.len();
                        let p = PathBuf::from(&path);
                        if let Some(bytes) = thumbs::lookup(&p, mtime, size) {
                            thumb_response(200, bytes)
                        } else {
                            // 未命中缓存才占生成许可（HDD 并发上限）
                            let sem = app.state::<state::AppState>().thumbs_sem.clone();
                            let permit = sem.acquire().await;
                            let bytes = tauri::async_runtime::spawn_blocking(move || {
                                thumbs::create(&p, mtime, size)
                            })
                            .await
                            .ok()
                            .flatten();
                            drop(permit);
                            match bytes {
                                Some(b) => thumb_response(200, b),
                                None => thumb_response(404, Vec::new()),
                            }
                        }
                    }
                    Err(_) => thumb_response(404, Vec::new()),
                };
                responder.respond(resp);
            });
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::set_library_root,
            commands::list_devices,
            commands::scan_card,
            commands::start_copy,
            commands::cancel_copy,
            commands::pause_copy,
            commands::library_stats,
            commands::home_overview,
            commands::import_log_clear,
            commands::set_last_page,
            commands::set_auto_scan,
            commands::open_in_explorer,
            commands::open_with_photoshop,
            commands::detect_photoshop,
            commands::set_photoshop_path,
            commands::gallery_list,
            commands::gallery_info,
            commands::gallery_open,
            commands::gallery_set_rating,
            commands::gallery_set_favorite,
            commands::ratings_db_path,
            commands::ratings_import_from_files,
            commands::ratings_export_sidecars
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
