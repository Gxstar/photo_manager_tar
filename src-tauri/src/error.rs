use serde::Serialize;
use std::fmt;

/// 应用错误：跨 IPC 边界一律返回此类型，前端拿到 { kind, message }
#[derive(Debug)]
pub enum AppError {
    LibraryUnreachable(String),
    DeviceDisconnected,
    TargetExists,
    IoError(String),
    TaskBusy,
    NoTask,
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppError::LibraryUnreachable(p) => write!(f, "媒体库路径不可访问：{p}"),
            AppError::DeviceDisconnected => write!(f, "卡片已拔出或不可读"),
            AppError::TargetExists => write!(f, "目标文件已存在，未覆盖"),
            AppError::IoError(m) => write!(f, "文件操作失败：{m}"),
            AppError::TaskBusy => write!(f, "已有拷贝任务在进行中"),
            AppError::NoTask => write!(f, "当前没有进行中的拷贝任务"),
        }
    }
}

impl AppError {
    pub fn kind_str(&self) -> &'static str {
        match self {
            AppError::LibraryUnreachable(_) => "libraryUnreachable",
            AppError::DeviceDisconnected => "deviceDisconnected",
            AppError::TargetExists => "targetExists",
            AppError::IoError(_) => "ioError",
            AppError::TaskBusy => "taskBusy",
            AppError::NoTask => "noTask",
        }
    }
}

impl Serialize for AppError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut st = serializer.serialize_struct("AppError", 2)?;
        st.serialize_field("kind", self.kind_str())?;
        st.serialize_field("message", &self.to_string())?;
        st.end()
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::NotFound {
            AppError::DeviceDisconnected
        } else {
            AppError::IoError(e.to_string())
        }
    }
}

pub type AppResult<T> = Result<T, AppError>;
