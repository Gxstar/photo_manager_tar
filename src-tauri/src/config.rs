use crate::error::{AppError, AppResult};
use crate::models::AppConfig;
use std::path::PathBuf;

/// 配置文件路径：%APPDATA%\com.gengx.sdcard-importer\config.json
fn config_path() -> PathBuf {
    let dir = dirs_data_dir().join("com.gengx.sdcard-importer");
    std::fs::create_dir_all(&dir).ok();
    dir.join("config.json")
}

/// %APPDATA%（Roaming）
pub fn dirs_data_dir() -> PathBuf {
    std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// 读取配置；文件缺失或损坏时返回默认值
pub fn load() -> AppConfig {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// 保存配置
pub fn save(cfg: &AppConfig) -> AppResult<()> {
    let s = serde_json::to_string_pretty(cfg).map_err(|e| AppError::IoError(e.to_string()))?;
    std::fs::write(config_path(), s).map_err(|e| AppError::IoError(e.to_string()))?;
    Ok(())
}
