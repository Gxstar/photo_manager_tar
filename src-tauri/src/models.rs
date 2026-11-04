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
    /// 已存在：目标存在且字节数相同，跳过
    Exists,
    /// 冲突：目标存在但字节数不同（多半是崩溃残留），跳过并报告
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
