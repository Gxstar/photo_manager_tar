use crate::error::AppError;
use crate::models::{CopyEvent, CopyPlan, FileStatus};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::ipc::Channel;

/// 拷贝缓冲 1 MB
const COPY_BUF: usize = 1024 * 1024;
/// 进度上报节流：100ms
const REPORT_INTERVAL: Duration = Duration::from_millis(100);

/// 任务控制标志（跨线程共享，pause/cancel 由独立命令置位）
pub struct CopyControl {
    pub pause: AtomicBool,
    pub cancel: AtomicBool,
}

impl CopyControl {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            pause: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
        })
    }
}

/// 3 秒滑动窗口速率计（首尾差值，避免瞬时抖动）
pub struct RateMeter {
    samples: VecDeque<(Instant, u64)>,
    window: Duration,
}

impl RateMeter {
    pub fn new() -> Self {
        Self {
            samples: VecDeque::new(),
            window: Duration::from_secs(3),
        }
    }

    pub fn bytes_per_sec(&mut self, now: Instant, total_done: u64) -> u64 {
        self.samples.push_back((now, total_done));
        while let Some(&(t, _)) = self.samples.front() {
            if now.duration_since(t) > self.window {
                self.samples.pop_front();
            } else {
                break;
            }
        }
        match (self.samples.front(), self.samples.back()) {
            (Some(&(t0, b0)), Some(&(t1, b1))) if t1 > t0 => {
                ((b1 - b0) as f64 / (t1 - t0).as_secs_f64()) as u64
            }
            _ => 0,
        }
    }
}

/// 拷贝执行入口：在 blocking 线程池里顺序拷贝全部 New 文件。
/// 单文件失败重试 1 次，仍失败记录并继续。
/// 一次拷贝的汇总结果（供导入日志落库；事件通道仍负责 UI 实时推送）
#[derive(Debug, Clone, Default)]
pub struct CopySummary {
    pub cancelled: bool,
    pub copied: usize,
    pub failed: usize,
    /// 实际写入字节数
    pub bytes_copied: u64,
    pub elapsed_secs: u64,
    pub avg_bytes_per_sec: u64,
    pub failures: Vec<(String, String)>,
}

pub fn run_copy(
    plan: CopyPlan,
    control: Arc<CopyControl>,
    channel: Channel<CopyEvent>,
) -> CopySummary {
    let started = Instant::now();
    let new_files: Vec<_> = plan
        .files
        .iter()
        .filter(|f| f.status == FileStatus::New)
        .cloned()
        .collect();

    let total_bytes: u64 = new_files.iter().map(|f| f.size).sum();
    let _ = channel.send(CopyEvent::Started {
        total_files: new_files.len(),
        total_bytes,
    });

    let mut buf = vec![0u8; COPY_BUF];
    let mut rate = RateMeter::new();
    let mut done_bytes: u64 = 0;
    let mut done_files: usize = 0;
    let mut copied: usize = 0;
    let mut failed: usize = 0;
    let mut last_report = Instant::now() - REPORT_INTERVAL;
    let mut pause_announced = false;
    let mut cancelled = false;
    let mut failures: Vec<(String, String)> = Vec::new();

    'outer: for f in &new_files {
        // 取消：跳出主循环（已拷文件天然保留，重扫幂等）
        if control.cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break 'outer;
        }
        // 暂停：轮询等待，恢复后继续
        while control.pause.load(Ordering::Relaxed) {
            if !pause_announced {
                let _ = channel.send(CopyEvent::Paused);
                pause_announced = true;
            }
            if control.cancel.load(Ordering::Relaxed) {
                cancelled = true;
                break 'outer;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if pause_announced {
            pause_announced = false;
        }

        // 拷贝单文件（失败重试 1 次）
        let mut result = Err(AppError::IoError("uninitialized".into()));
        for attempt in 0..2 {
            result = copy_one(&f.source_path, &f.target_path, &mut buf);
            if result.is_ok() {
                break;
            }
            if attempt == 0 {
                std::thread::sleep(Duration::from_millis(150));
            }
        }

        match result {
            Ok(n) => {
                copied += 1;
                done_files += 1;
                done_bytes += n;
            }
            Err(e) => {
                failed += 1;
                done_files += 1;
                let reason = e.to_string();
                failures.push((f.file_name.clone(), reason.clone()));
                let _ = channel.send(CopyEvent::FileFailed {
                    file_name: f.file_name.clone(),
                    reason,
                });
            }
        }

        // 节流上报进度
        let now = Instant::now();
        if now.duration_since(last_report) >= REPORT_INTERVAL || done_files == new_files.len() {
            last_report = now;
            let bps = rate.bytes_per_sec(now, done_bytes);
            let remaining = total_bytes.saturating_sub(done_bytes);
            let eta = if bps > 0 {
                Some(remaining / bps)
            } else {
                None
            };
            let _ = channel.send(CopyEvent::Progress {
                done_files,
                done_bytes,
                current_file: f.file_name.clone(),
                bytes_per_sec: bps,
                eta_secs: eta,
            });
        }
    }

    let elapsed = started.elapsed().as_secs().max(1);
    let avg_bytes_per_sec = done_bytes / elapsed;
    if cancelled {
        let _ = channel.send(CopyEvent::Cancelled);
    } else {
        let _ = channel.send(CopyEvent::Finished {
            copied,
            skipped: 0,
            failed,
            elapsed_secs: elapsed,
            avg_bytes_per_sec,
        });
    }
    CopySummary {
        cancelled,
        copied,
        failed,
        bytes_copied: done_bytes,
        elapsed_secs: elapsed,
        avg_bytes_per_sec,
        failures,
    }
}

/// 单文件拷贝：create_new 独占创建目标 + 分块直写 + sync_all。
///
/// 实测依据：Windows 的 fs::rename 会静默覆盖已存在文件（返回 Ok 且原内容被替换），
/// 因此禁用「临时文件 + rename」；create_new 撞已存在时返回 AlreadyExists，
/// 原文件分毫不动，天然满足「绝不覆盖」。
///
/// 写入中途失败时，删除本次自己刚创建的半截目标（重试才能成功）；
/// 进程被强杀留下的残留由下次扫描的「冲突」判定兜底。
pub(crate) fn copy_one(src: &Path, target: &Path, buf: &mut [u8]) -> Result<u64, AppError> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(AppError::from)?;
    }
    let mut input =
        BufReader::with_capacity(COPY_BUF, File::open(src).map_err(AppError::from)?);

    // 独占创建：已存在 → 拒绝覆盖
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                AppError::TargetExists
            } else {
                AppError::IoError(e.to_string())
            }
        })?;
    let mut output = BufWriter::new(output);

    let mut total: u64 = 0;
    let write_result: Result<(), AppError> = (|| {
        loop {
            let n = input.read(buf).map_err(AppError::from)?;
            if n == 0 {
                break;
            }
            output
                .write_all(&buf[..n])
                .map_err(|e| AppError::IoError(e.to_string()))?;
            total += n as u64;
        }
        let file = output
            .into_inner()
            .map_err(|e| AppError::IoError(e.to_string()))?;
        file.sync_all().map_err(|e| AppError::IoError(e.to_string()))?;
        Ok(())
    })();

    if let Err(e) = write_result {
        // 清理本次创建的半截文件（best-effort）
        let _ = std::fs::remove_file(target);
        return Err(e);
    }
    Ok(total)
}
