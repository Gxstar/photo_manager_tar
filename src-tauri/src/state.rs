use crate::config;
use crate::copier::CopyControl;
use crate::gallery::TimeCache;
use crate::meta_store::MetaStore;
use crate::models::AppConfig;
use crate::smart::Smart;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

/// 全局状态：配置 + 当前拷贝任务控制 + 图库状态
pub struct AppState {
    pub config: Mutex<AppConfig>,
    pub control: Mutex<Option<Arc<CopyControl>>>,
    /// 图库拍摄时间缓存：path → (mtime_secs, "YYYY-MM-DD HH:MM:SS")（可重建缓存）
    pub time_cache: Mutex<TimeCache>,
    /// 图库 SQLite 持久层：时间缓存（可重建）+ ratings（评分权威源，用户数据）
    pub meta: MetaStore,
    /// 图库时间补齐任务代号：每次 gallery_list 自增，旧任务批间检查后退出
    pub gallery_gen: AtomicU64,
    /// 缩略图生成并发限制（HDD 友好）
    pub thumbs_sem: Arc<tokio::sync::Semaphore>,
    /// 媒体库文件系统监听（热刷新）；变更库根时顶替重启
    pub watcher: Mutex<Option<crate::watch::WatcherHandle>>,
    /// 智能相册独立模块：独立 smart.db，删掉即回到无 AI 状态（§17.6）
    pub smart: Smart,
}

impl AppState {
    pub fn new() -> Self {
        // 时间缓存启动灌入内存（评分不再进内存缓存，由 gallery_list 直接从 DB 读）
        let meta = MetaStore::open();
        let times = meta.load_times();
        let rating_count = meta.all_ratings().len();
        eprintln!(
            "[meta_store] 启动装载：时间 {} 项 / 评分 {} 项",
            times.len(),
            rating_count
        );
        Self {
            config: Mutex::new(config::load()),
            control: Mutex::new(None),
            time_cache: Mutex::new(times),
            meta,
            gallery_gen: AtomicU64::new(0),
            thumbs_sem: Arc::new(tokio::sync::Semaphore::new(3)),
            watcher: Mutex::new(None),
            smart: Smart::open(),
        }
    }

    pub fn update_config(&self, cfg: AppConfig) {
        *self.config.lock().unwrap() = cfg;
    }
}
