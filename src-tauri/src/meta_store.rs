//! 图库元数据持久缓存（SQLite）。
//!
//! 表 `meta_cache`：(path, mtime) → (shoot_time, rating)。行存在即「已探测」：
//! - shoot_time NULL = 探测过但该文件无权威时间
//! - rating NULL = 非 JPEG（不适用）；0 = JPEG 已确认无评分；1-5 = 评分
//!
//! 生命周期：应用启动 `open` + `load_all` 灌入进程内缓存；补齐批次与打分
//! 成功后 UPSERT 回写。打开失败时降级为纯内存模式（写入 no-op），不阻塞应用。

use crate::gallery::TimePatch;
use crate::gallery::{RatingCache, TimeCache};
use crate::models::{ImportFailure, ImportLog};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct MetaStore {
    conn: Option<Mutex<rusqlite::Connection>>,
}

/// 数据库路径：%LOCALAPPDATA%\com.gengx.develop\gallery.db（与缩略图缓存同目录）
fn db_path() -> PathBuf {
    std::env::var("LOCALAPPDATA")
        .map(|d| PathBuf::from(d).join("com.gengx.develop").join("gallery.db"))
        .unwrap_or_else(|_| PathBuf::from("gallery.db"))
}

impl MetaStore {
    /// 打开数据库并建表；失败返回降级实例（不持久化）
    pub fn open() -> Self {
        let path = db_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = rusqlite::Connection::open(&path)
            .and_then(|c| {
                c.execute_batch(
                    "CREATE TABLE IF NOT EXISTS meta_cache (
                        path       TEXT PRIMARY KEY,
                        mtime      INTEGER NOT NULL,
                        shoot_time TEXT,
                        rating     INTEGER
                    ) WITHOUT ROWID;
                    CREATE TABLE IF NOT EXISTS import_log (
                        id                 INTEGER PRIMARY KEY AUTOINCREMENT,
                        finished_at        TEXT NOT NULL,
                        device_path        TEXT NOT NULL,
                        library_root       TEXT NOT NULL,
                        files_total        INTEGER NOT NULL,
                        files_copied       INTEGER NOT NULL,
                        files_skipped      INTEGER NOT NULL,
                        files_conflict     INTEGER NOT NULL,
                        files_failed       INTEGER NOT NULL,
                        bytes_copied       INTEGER NOT NULL,
                        elapsed_secs       INTEGER NOT NULL,
                        avg_bytes_per_sec  INTEGER NOT NULL,
                        cancelled          INTEGER NOT NULL DEFAULT 0
                    );
                    CREATE TABLE IF NOT EXISTS import_failure (
                        id        INTEGER PRIMARY KEY AUTOINCREMENT,
                        log_id    INTEGER NOT NULL,
                        file_name TEXT NOT NULL,
                        reason    TEXT NOT NULL
                    );
                    CREATE INDEX IF NOT EXISTS idx_import_failure_log
                        ON import_failure(log_id);",
                )?;
                Ok(c)
            })
            .map(Mutex::new)
            .map_err(|e| eprintln!("[meta_store] 打开失败，降级纯内存: {e}"));
        match conn {
            Ok(m) => Self { conn: Some(m) },
            Err(_) => Self { conn: None },
        }
    }

    /// 启动装载：返回 (时间缓存, 评分缓存)，与进程内缓存结构一致
    pub fn load_all(&self) -> (TimeCache, RatingCache) {
        let mut times = HashMap::new();
        let mut ratings = HashMap::new();
        let Some(conn) = &self.conn else {
            return (times, ratings);
        };
        let Ok(db) = conn.lock() else { return (times, ratings) };
        let Ok(mut stmt) =
            db.prepare("SELECT path, mtime, shoot_time, rating FROM meta_cache")
        else {
            return (times, ratings);
        };
        let _ = stmt.query_map([], |row| {
            let path: String = row.get(0)?;
            let mtime: i64 = row.get(1)?;
            let shoot_time: Option<String> = row.get(2)?;
            let rating: Option<i64> = row.get(3)?;
            Ok((path, mtime, shoot_time, rating))
        })
        .map(|rows| {
            for row in rows.flatten() {
                let (path, mtime, shoot_time, rating) = row;
                times.insert(path.clone(), (mtime, shoot_time));
                if let Some(r) = rating.filter(|r| (0..=5).contains(r)) {
                    ratings.insert(path, (mtime, (r > 0).then_some(r as u8)));
                }
            }
        });
        (times, ratings)
    }

    /// 补齐批次回写：时间 + 评分一并 UPSERT（单事务）
    pub fn upsert_patches(&self, patches: &[TimePatch]) {
        let Some(conn) = &self.conn else { return };
        let Ok(db) = conn.lock() else { return };
        let _ = db.execute_batch("BEGIN;");
        if let Ok(mut stmt) = db.prepare_cached(
            "INSERT INTO meta_cache (path, mtime, shoot_time, rating) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(path) DO UPDATE SET mtime = excluded.mtime,
                 shoot_time = excluded.shoot_time, rating = excluded.rating",
        ) {
            for p in patches {
                let ratable = crate::meta::xmp::is_ratable(Path::new(&p.path));
                // 可评分文件：0 = 已确认无评分（区别于 NULL = 类型不支持）
                let rating = if ratable { Some(i64::from(p.rating.unwrap_or(0))) } else { None };
                let _ = stmt.execute(rusqlite::params![p.path, p.mtime_secs, p.shoot_time, rating]);
            }
        }
        let _ = db.execute_batch("COMMIT;");
    }

    /// 打分成功后的单条回写（rating None = 清除，存 0）
    pub fn upsert_rating(&self, path: &str, mtime: i64, rating: Option<u8>) {
        let Some(conn) = &self.conn else { return };
        let Ok(db) = conn.lock() else { return };
        let _ = db
            .prepare_cached(
                "INSERT INTO meta_cache (path, mtime, shoot_time, rating) VALUES (?1, ?2, NULL, ?3)
                 ON CONFLICT(path) DO UPDATE SET mtime = excluded.mtime, rating = excluded.rating",
            )
            .and_then(|mut s| {
                s.execute(rusqlite::params![path, mtime, i64::from(rating.unwrap_or(0))])
            });
    }

    // ── 导入日志 ─────────────────────────────────────────────────────────

    /// 写入一条拷贝记录（含失败明细），返回自增 id；降级模式返回 -1
    pub fn log_import(&self, rec: &ImportLog) -> i64 {
        let Some(conn) = &self.conn else { return -1 };
        let Ok(db) = conn.lock() else { return -1 };
        let Ok(id) = db
            .prepare_cached(
                "INSERT INTO import_log (
                    finished_at, device_path, library_root, files_total, files_copied,
                    files_skipped, files_conflict, files_failed, bytes_copied,
                    elapsed_secs, avg_bytes_per_sec, cancelled
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            )
            .and_then(|mut s| {
                s.execute(rusqlite::params![
                    rec.finished_at,
                    rec.device_path,
                    rec.library_root,
                    rec.files_total as i64,
                    rec.files_copied as i64,
                    rec.files_skipped as i64,
                    rec.files_conflict as i64,
                    rec.files_failed as i64,
                    rec.bytes_copied as i64,
                    rec.elapsed_secs as i64,
                    rec.avg_bytes_per_sec as i64,
                    i64::from(rec.cancelled),
                ])
            })
            .map(|_| db.last_insert_rowid())
        else {
            return -1;
        };
        if id > 0 && !rec.failures.is_empty() {
            if let Ok(mut stmt) = db.prepare_cached(
                "INSERT INTO import_failure (log_id, file_name, reason) VALUES (?1, ?2, ?3)",
            ) {
                for f in &rec.failures {
                    let _ = stmt.execute(rusqlite::params![id, f.file_name, f.reason]);
                }
            }
        }
        id
    }

    /// 最近 N 条导入记录（新 → 旧），带失败明细
    pub fn recent_imports(&self, limit: usize) -> Vec<ImportLog> {
        let Some(conn) = &self.conn else { return Vec::new() };
        let Ok(db) = conn.lock() else { return Vec::new() };
        let mut out = Vec::new();
        {
            let Ok(mut stmt) = db.prepare(
                "SELECT id, finished_at, device_path, library_root, files_total, files_copied,
                        files_skipped, files_conflict, files_failed, bytes_copied, elapsed_secs,
                        avg_bytes_per_sec, cancelled
                 FROM import_log ORDER BY id DESC LIMIT ?1",
            ) else {
                return Vec::new();
            };
            let Ok(rows) = stmt.query_map([limit as i64], |r| {
                Ok(ImportLog {
                    id: r.get(0)?,
                    finished_at: r.get(1)?,
                    device_path: r.get(2)?,
                    library_root: r.get(3)?,
                    files_total: r.get::<_, i64>(4)? as usize,
                    files_copied: r.get::<_, i64>(5)? as usize,
                    files_skipped: r.get::<_, i64>(6)? as usize,
                    files_conflict: r.get::<_, i64>(7)? as usize,
                    files_failed: r.get::<_, i64>(8)? as usize,
                    bytes_copied: r.get::<_, i64>(9)? as u64,
                    elapsed_secs: r.get::<_, i64>(10)? as u64,
                    avg_bytes_per_sec: r.get::<_, i64>(11)? as u64,
                    cancelled: r.get::<_, i64>(12)? != 0,
                    failures: Vec::new(),
                })
            }) else {
                return Vec::new();
            };
            out.extend(rows.flatten());
        }
        for log in &mut out {
            let Ok(mut stmt) =
                db.prepare("SELECT file_name, reason FROM import_failure WHERE log_id = ?1")
            else {
                break;
            };
            if let Ok(rows) = stmt.query_map([log.id], |r| {
                Ok(ImportFailure {
                    file_name: r.get(0)?,
                    reason: r.get(1)?,
                })
            }) {
                log.failures = rows.flatten().collect();
            }
        }
        out
    }

    /// 清空导入日志，返回删除的条数
    pub fn clear_import_log(&self) -> usize {
        let Some(conn) = &self.conn else { return 0 };
        let Ok(db) = conn.lock() else { return 0 };
        let n: i64 = db
            .query_row("SELECT COUNT(*) FROM import_log", [], |r| r.get(0))
            .unwrap_or(0);
        let _ = db.execute("DELETE FROM import_failure", []);
        let _ = db.execute("DELETE FROM import_log", []);
        n as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(name: &str) -> (MetaStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!("sdi-metastore-{}-{name}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("gallery.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta_cache (
                path TEXT PRIMARY KEY, mtime INTEGER NOT NULL, shoot_time TEXT, rating INTEGER
            ) WITHOUT ROWID;
            CREATE TABLE IF NOT EXISTS import_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT, finished_at TEXT NOT NULL,
                device_path TEXT NOT NULL, library_root TEXT NOT NULL, files_total INTEGER NOT NULL,
                files_copied INTEGER NOT NULL, files_skipped INTEGER NOT NULL,
                files_conflict INTEGER NOT NULL, files_failed INTEGER NOT NULL,
                bytes_copied INTEGER NOT NULL, elapsed_secs INTEGER NOT NULL,
                avg_bytes_per_sec INTEGER NOT NULL, cancelled INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS import_failure (
                id INTEGER PRIMARY KEY AUTOINCREMENT, log_id INTEGER NOT NULL,
                file_name TEXT NOT NULL, reason TEXT NOT NULL
            );",
        )
        .unwrap();
        // 直接构造实例（绕过固定 db 路径）
        (MetaStore { conn: Some(Mutex::new(conn)) }, dir)
    }

    #[test]
    fn roundtrip_and_overwrite() {
        let (store, dir) = temp_store("rt");
        store.upsert_patches(&[
            TimePatch {
                path: "a.jpg".into(),
                mtime_secs: 100,
                shoot_time: Some("2026-01-01 10:00:00".into()),
                rating: Some(4),
            },
            TimePatch {
                path: "b.RW2".into(),
                mtime_secs: 200,
                shoot_time: Some("2026-02-02 11:00:00".into()),
                rating: None,
            },
            TimePatch {
                path: "c.mp4".into(),
                mtime_secs: 400,
                shoot_time: Some("2026-03-03 12:00:00".into()),
                rating: None,
            },
        ]);
        store.upsert_patches(&[TimePatch {
            path: "a.jpg".into(),
            mtime_secs: 300, // 重新打分后 mtime 变化
            shoot_time: Some("2026-01-01 10:00:00".into()),
            rating: Some(5),
        }]);
        store.upsert_rating("a.jpg", 300, None); // 清除评分

        let (times, ratings) = store.load_all();
        assert_eq!(times.get("a.jpg"), Some(&(300, Some("2026-01-01 10:00:00".into()))));
        assert_eq!(times.get("b.RW2"), Some(&(200, Some("2026-02-02 11:00:00".into()))));
        // 清除后 = 已探测无评分（0 → None）
        assert_eq!(ratings.get("a.jpg"), Some(&(300, None)));
        // RW2 现在也可评分：None → 0（已探测无评分），进入评分缓存
        assert_eq!(ratings.get("b.RW2"), Some(&(200, None)));
        // 视频等不可评分类型写 NULL，不进评分缓存
        assert!(!ratings.contains_key("c.mp4"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn degraded_mode_no_panic() {
        let store = MetaStore { conn: None };
        store.upsert_patches(&[]);
        store.upsert_rating("x", 1, Some(3));
        let (t, r) = store.load_all();
        assert!(t.is_empty() && r.is_empty());
        // 导入日志在降级模式下也要安全
        assert_eq!(store.log_import(&sample_log()), -1);
        assert!(store.recent_imports(5).is_empty());
        assert_eq!(store.clear_import_log(), 0);
    }

    fn sample_log() -> ImportLog {
        ImportLog {
            id: 0,
            finished_at: "2026-11-04 20:30:00".into(),
            device_path: "J:\\".into(),
            library_root: "D:\\媒体\\图片".into(),
            files_total: 440,
            files_copied: 439,
            files_skipped: 1,
            files_conflict: 0,
            files_failed: 1,
            bytes_copied: 6_900_000_000,
            elapsed_secs: 92,
            avg_bytes_per_sec: 75_000_000,
            cancelled: false,
            failures: vec![ImportFailure {
                file_name: "P1244742.MP4".into(),
                reason: "拒绝访问".into(),
            }],
        }
    }

    #[test]
    fn import_log_roundtrip_and_clear() {
        let (store, dir) = temp_store("import-log-rt");
        let id = store.log_import(&sample_log());
        assert!(id > 0);

        let logs = store.recent_imports(5);
        assert_eq!(logs.len(), 1);
        let l = &logs[0];
        assert_eq!(l.files_copied, 439);
        assert_eq!(l.files_failed, 1);
        assert_eq!(l.bytes_copied, 6_900_000_000);
        assert!(!l.cancelled);
        assert_eq!(l.failures.len(), 1);
        assert_eq!(l.failures[0].file_name, "P1244742.MP4");

        // 第二条在前
        let mut second = sample_log();
        second.cancelled = true;
        second.files_copied = 10;
        assert!(store.log_import(&second) > id);
        let logs = store.recent_imports(5);
        assert_eq!(logs.len(), 2);
        assert!(logs[0].cancelled);
        assert_eq!(logs[1].id, id);

        assert_eq!(store.clear_import_log(), 2);
        assert!(store.recent_imports(5).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
