use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 文件类别（按扩展名分类）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FileKind {
    Image,
    Raw,
    Video,
    Sidecar,
    Unknown,
}

/// 拍摄时间来源（可信度从高到低）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TimeSource {
    ExifDateTimeOriginal,
    ExifDateTimeDigitized,
    FileNameFull,
    FileNameDate,
    FileSystemCreated,
    FileSystemModified,
    None,
}

/// 拷贝计划中单文件的状态（三态 + 结果态）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FileStatus {
    /// 新增：目标不存在，待拷贝
    New,
    /// 已存在：卡上快门产物已落库（库内字节数 ≥ 卡上；更小的 JPEG 若尾部完整，
    /// 即裁剪等编辑缩小，也算）。跳过
    Exists,
    /// 冲突：库内比卡上更小且不是完整 JPEG（多半是进程强杀残留），跳过并报告
    Conflict,
    /// 改名跳过：库内已有内容相同但改了名的文件
    /// （EXIF 拍摄秒 + 字节数 + 扩展名签名一致），跳过并在 note 里标注库内路径
    SuspectSkipped,
    /// 已拷贝
    Copied,
    /// 失败
    Failed,
}

/// 卡片上的一个文件（扫描产物）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaFile {
    pub source_path: PathBuf,
    pub file_name: String,
    pub ext: String,
    pub size: u64,
    pub kind: FileKind,
    /// "YYYY-MM-DD HH:MM:SS"，None 表示无法确定
    pub shoot_time: Option<String>,
    pub time_source: TimeSource,
    pub target_path: PathBuf,
    /// 归属月份组 "2026-10"，待定为 "待定"
    pub month_group: String,
    pub status: FileStatus,
    pub note: Option<String>,
}

/// 可移动设备
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    /// 盘符根路径，如 "I:\"
    pub path: String,
    pub label: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
    /// 容量为 0 → 读卡器无卡
    pub has_media: bool,
    pub is_camera_card: bool,
    /// 命中的相机特征目录，如 "DCIM"
    pub card_kind: Option<String>,
}

/// 拷贝计划（扫描结果）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyPlan {
    pub device_path: String,
    pub library_root: String,
    pub files: Vec<MediaFile>,
    pub total_files: usize,
    pub new_files: usize,
    pub new_bytes: u64,
    pub exists_files: usize,
    pub conflict_files: usize,
    /// 改名跳过数（库内已有内容相同但改了名的文件）
    pub suspect_files: usize,
    /// 目标盘剩余空间
    pub free_bytes: u64,
    pub space_ok: bool,
}

/// 扫描进度事件（经 Channel 推送；扫描在后台线程跑，不阻塞 UI）
#[derive(Debug, Clone, Serialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "event",
    content = "data"
)]
pub enum ScanEvent {
    /// 逐文件解析进度（首个事件携带总数）
    Progress { done: usize, total: usize, current_file: String },
    /// 扫描完成，携带完整计划
    Finished { plan: CopyPlan },
    /// 扫描失败
    Failed { message: String },
}

/// 拷贝进度事件（经 tauri::ipc::Channel 推送到前端）
#[derive(Debug, Clone, Serialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "event",
    content = "data"
)]
pub enum CopyEvent {
    Started {
        total_files: usize,
        total_bytes: u64,
    },
    Progress {
        done_files: usize,
        done_bytes: u64,
        current_file: String,
        bytes_per_sec: u64,
        eta_secs: Option<u64>,
    },
    FileFailed {
        file_name: String,
        reason: String,
    },
    Paused,
    Cancelled,
    Finished {
        copied: usize,
        skipped: usize,
        failed: usize,
        elapsed_secs: u64,
        avg_bytes_per_sec: u64,
    },
}

/// 冲突策略（当前固定跳过，预留字段）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConflictPolicy {
    Skip,
}

/// 应用配置（持久化到 %APPDATA%\{identifier}\config.json）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppConfig {
    pub library_root: String,
    pub original_dir: String,
    pub video_dir: String,
    pub pending_dir: String,
    pub conflict_policy: ConflictPolicy,
    /// 上次停留的页面 id（copy / gallery / settings）。
    /// 靠结构体级 `#[serde(default)]` 兜底：旧 config.json 缺此字段时取 Default，
    /// 不会让整份配置反序列化失败（否则 library_root 会被静默丢弃）
    pub last_page: String,
    /// 插卡后自动扫描（只读卡、不改卡、不自动拷贝）
    pub auto_scan: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            // 首次使用引导用户在设置页选择媒体库根目录
            library_root: String::new(),
            original_dir: "原图".to_string(),
            video_dir: "视频".to_string(),
            pending_dir: "待定".to_string(),
            conflict_policy: ConflictPolicy::Skip,
            last_page: "copy".to_string(),
            auto_scan: true,
        }
    }
}

/// 媒体库统计
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryStats {
    pub total_files: usize,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

// ── 首页（无卡空态）────────────────────────────────────────────────────

/// 单个月份目录的归档统计
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MonthStat {
    /// "2026-10"
    pub label: String,
    /// 该月「原图」目录绝对路径（不存在则给「视频」目录），供点击打开
    pub dir: String,
    /// 媒体文件数（只计 Image/Raw/Video，与图库口径一致）
    pub files: usize,
    pub bytes: u64,
}

/// 最近导入的一张（按 mtime 降序 = 导入时间降序）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentItem {
    pub path: String,
    pub name: String,
    pub kind: FileKind,
    pub mtime_secs: i64,
}

/// 首页空态数据
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HomeOverview {
    pub months: Vec<MonthStat>,
    pub recent: Vec<RecentItem>,
    /// 最近几次导入记录（来自 SQLite `import_log`）
    pub imports: Vec<ImportLog>,
}

// ── 导入日志 ─────────────────────────────────────────────────────────────

/// 一次拷贝的失败明细
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportFailure {
    pub file_name: String,
    pub reason: String,
}

/// 一次拷贝记录（每次点「开始拷贝」产生一条；取消也记，`cancelled = true`）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportLog {
    pub id: i64,
    /// "YYYY-MM-DD HH:MM:SS"（本地时间，拷贝结束时定）
    pub finished_at: String,
    pub device_path: String,
    pub library_root: String,
    /// 计划内文件总数
    pub files_total: usize,
    /// 实际拷贝成功数
    pub files_copied: usize,
    /// 已存在跳过数
    pub files_skipped: usize,
    /// 冲突 + 改名跳过数
    pub files_conflict: usize,
    pub files_failed: usize,
    /// 实际写入字节数（不是计划字节数）
    pub bytes_copied: u64,
    pub elapsed_secs: u64,
    pub avg_bytes_per_sec: u64,
    pub cancelled: bool,
    pub failures: Vec<ImportFailure>,
}
