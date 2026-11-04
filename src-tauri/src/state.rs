use crate::config;
use crate::copier::CopyControl;
use crate::models::AppConfig;
use std::sync::{Arc, Mutex};

/// 全局状态：配置 + 当前拷贝任务控制
pub struct AppState {
    pub config: Mutex<AppConfig>,
    pub control: Mutex<Option<Arc<CopyControl>>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            config: Mutex::new(config::load()),
            control: Mutex::new(None),
        }
    }

    pub fn update_config(&self, cfg: AppConfig) {
        *self.config.lock().unwrap() = cfg;
    }
}
