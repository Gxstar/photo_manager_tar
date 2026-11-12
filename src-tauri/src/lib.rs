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
mod recycle;
mod raw_preview;
mod scanner;
mod smart;
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
    // 200 的缓存键已含 mtime+size，文件变更即换键 → 内容可视为永久有效。
    // 404 往往是暂时失败（Shell 路径异常、RAW 解码器缺失、缓存尚未生成），
    // 必须 no-store：否则修复后浏览器在一年内都不会重新请求，问题看起来「没修好」。
    let cache = if status == 200 {
        "max-age=31536000, immutable"
    } else {
        "no-store"
    };
    Response::builder()
        .status(status)
        .header("Content-Type", "image/jpeg")
        .header("Cache-Control", cache)
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
            // ONNX Runtime 动态库：优先打包后的 resource_dir，开发期回退到 crate 根。
            // 不从 pyke CDN 下载（国内不可用），DLL 随应用分发。
            let dll = app
                .path()
                .resource_dir()
                .ok()
                .map(|d| d.join("onnxruntime.dll"))
                .filter(|p| p.is_file())
                .unwrap_or_else(|| {
                    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/onnxruntime.dll")
                });
            smart::clip::set_dll(dll);
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
        // 底图只用高德瓦片（前端直连，免 key）。天地图瓦片曾接过，按用户要求已移除（docs §29）。
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
            commands::gallery_set_rejected,
            commands::gallery_delete_rejected,
            commands::ratings_db_path,
            commands::ratings_import_from_files,
            commands::ratings_export_sidecars,
            smart::smart_model_status,
            smart::smart_download_model,
            smart::smart_cancel_download,
            smart::smart_delete_model,
            smart::smart_index_start,
            smart::smart_index_cancel,
            smart::smart_search,
            smart::smart_things,
            smart::smart_thing_photos,
            smart::smart_backfill_tags,
            smart::smart_face_scan,
            smart::smart_face_cancel,
            smart::smart_face_auto_start,
            smart::smart_face_auto_scan_get,
            smart::smart_set_face_auto_scan,
            smart::smart_face_clusters,
            smart::smart_face_cluster_photos,
            smart::smart_face_name,
            smart::smart_face_ignore,
            smart::smart_face_clear,
            smart::smart_places,
            smart::smart_ingest_gps,
            smart::smart_db_path,
            smart::smart_open_models_dir,
            smart::smart_scan_gps,
            smart::smart_rename_album,
            smart::smart_set_cover,
            smart::smart_album_photos,
            smart::smart_get_settings,
            smart::smart_set_tianditu_key,
            smart::smart_set_place_level,
            smart::smart_geocode_places,
            smart::smart_group_photos,
            smart::smart_photo_location,
            smart::smart_group_overrides,
            smart::smart_set_group_override,
            smart::smart_people,
            smart::smart_create_person,
            smart::smart_rename_person,
            smart::smart_delete_person,
            smart::smart_person_photos,
            smart::smart_person_link,
            smart::smart_person_unlink,
            smart::smart_set_person_cover,
            smart::smart_photo_people
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
