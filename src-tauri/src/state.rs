use crate::config;
use crate::copier::CopyControl;
use crate::gallery::{RatingCache, TimeCache};
use crate::meta_store::MetaStore;
use crate::models::AppConfig;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

/// 全局状态：配置 + 当前拷贝任务控制 + 图库状态
pub struct AppState {
    pub config: Mutex<AppConfig>,
    pub control: Mutex<Option<Arc<CopyControl>>>,
    /// 图库拍摄时间缓存：path → (mtime_secs, "YYYY-MM-DD HH:MM:SS")
    pub time_cache: Mutex<TimeCache>,
    /// 图库评分缓存：path → (mtime_secs, rating)；条目存在即已探测
    pub rating_cache: Mutex<RatingCache>,
    /// 图库元数据 SQLite 持久层（启动时已灌入上面两个内存缓存）
    pub meta: MetaStore,
    /// 图库时间补齐任务代号：每次 gallery_list 自增，旧任务批间检查后退出
    pub gallery_gen: AtomicU64,
    /// 缩略图生成并发限制（HDD 友好）
    pub thumbs_sem: Arc<tokio::sync::Semaphore>,
    /// 媒体库文件系统监听（热刷新）；变更库根时顶替重启
    pub watcher: Mutex<Option<crate::watch::WatcherHandle>>,
}

impl AppState {
    pub fn new() -> Self {
        // SQLite 持久缓存 → 灌入内存缓存：日常启动图库秒出，无需重新解析
        let meta = MetaStore::open();
        let (times, ratings) = meta.load_all();
        eprintln!(
            "[meta_store] 启动装载：时间 {} 项 / 评分 {} 项",
            times.len(),
            ratings.len()
        );
        Self {
            config: Mutex::new(config::load()),
            control: Mutex::new(None),
            time_cache: Mutex::new(times),
            rating_cache: Mutex::new(ratings),
            meta,
            gallery_gen: AtomicU64::new(0),
            thumbs_sem: Arc::new(tokio::sync::Semaphore::new(3)),
            watcher: Mutex::new(None),
        }
    }

    pub fn update_config(&self, cfg: AppConfig) {
        *self.config.lock().unwrap() = cfg;
    }
}
