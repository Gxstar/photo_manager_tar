//! 图库 SQLite 持久层（docs/开发方案.md §10）。
//!
//! 两类数据**分表**，生命周期不同：
//! - `meta_cache`：拍摄时间缓存（path → mtime/shoot_time），**可重建**，删了会重扫。
//! - `ratings`：评分**权威源**（path → 1-5），用户数据，删了不可恢复。
//!   无评分 = 无行，清除评分 = DELETE；绝不写回图片文件。
//! - `import_log` / `import_failure`：每次拷贝的汇总与失败明细。
//!
//! 旧库文件的 `meta_cache.rating` 列保留但忽略（nullable，INSERT 不指定即可），
//! 因此不需要迁移脚本。DB 打开失败时降级为纯内存：缓存类操作 no-op，
//! 但**评分写入必须报错**（硬规则 #14），由命令层转成前端可见的错误。

use crate::gallery::{TimeCache, TimePatch};
use crate::models::{ImportFailure, ImportLog};
use std::collections::HashMap;
use std::path::PathBuf;
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
                        shoot_time TEXT
                    ) WITHOUT ROWID;
                    CREATE TABLE IF NOT EXISTS ratings (
                        path       TEXT PRIMARY KEY,
                        rating     INTEGER NOT NULL CHECK (rating BETWEEN 1 AND 5),
                        updated_at TEXT NOT NULL
                    ) WITHOUT ROWID;
                    CREATE TABLE IF NOT EXISTS favorites (
                        path       TEXT PRIMARY KEY,
                        created_at TEXT NOT NULL
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

    /// 评分是否可持久化。false 时命令层必须报错，不允许静默成功
    pub fn is_available(&self) -> bool {
        self.conn.is_some()
    }

    /// 供设置页展示：备份时要复制的就是这个文件
    pub fn db_location(&self) -> String {
        db_path().to_string_lossy().to_string()
    }

    /// 启动装载：拍摄时间缓存
    pub fn load_times(&self) -> TimeCache {
        let mut times = HashMap::new();
        let Some(conn) = &self.conn else { return times };
        let Ok(db) = conn.lock() else { return times };
        let Ok(mut stmt) = db.prepare("SELECT path, mtime, shoot_time FROM meta_cache") else {
            return times;
        };
        let _ = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .map(|rows| {
                for row in rows.flatten() {
                    let (path, mtime, shoot_time) = row;
                    times.insert(path, (mtime, shoot_time));
                }
            });
        times
    }

    /// 评分权威值：path → 1-5（无评分的文件不在表里）
    pub fn all_ratings(&self) -> HashMap<String, u8> {
        let mut out = HashMap::new();
        let Some(conn) = &self.conn else { return out };
        let Ok(db) = conn.lock() else { return out };
        let Ok(mut stmt) = db.prepare("SELECT path, rating FROM ratings") else {
            return out;
        };
        let _ = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map(|rows| {
                for row in rows.flatten() {
                    let (path, r) = row;
                    if (1..=5).contains(&r) {
                        out.insert(path, r as u8);
                    }
                }
            });
        out
    }

    /// 时间补齐批次回写（单事务，不含评分）
    pub fn upsert_patches(&self, patches: &[TimePatch]) {
        let Some(conn) = &self.conn else { return };
        let Ok(db) = conn.lock() else { return };
        let _ = db.execute_batch("BEGIN;");
        if let Ok(mut stmt) = db.prepare_cached(
            "INSERT INTO meta_cache (path, mtime, shoot_time) VALUES (?1, ?2, ?3)
             ON CONFLICT(path) DO UPDATE SET mtime = excluded.mtime,
                 shoot_time = excluded.shoot_time",
        ) {
            for p in patches {
                let _ = stmt.execute(rusqlite::params![p.path, p.mtime_secs, p.shoot_time]);
            }
        }
        let _ = db.execute_batch("COMMIT;");
    }

    /// 批量打分 / 清除评分（单个事务）。rating=None = 删除这些行。
    /// DB 不可用或写失败一律返回 Err —— 绝不假装保存成功。
    pub fn set_ratings_batch(
        &self,
        paths: &[String],
        rating: Option<u8>,
    ) -> Result<usize, String> {
        let Some(conn) = &self.conn else {
            return Err("评分数据库不可用，评分未保存".into());
        };
        let mut db = conn
            .lock()
            .map_err(|_| "评分数据库被占用，评分未保存".to_string())?;
        let tx = db
            .transaction()
            .map_err(|e| format!("评分保存失败: {e}"))?;
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let n = match rating {
            Some(v) => {
                let mut stmt = tx
                    .prepare_cached(
                        "INSERT INTO ratings (path, rating, updated_at) VALUES (?1, ?2, ?3)
                         ON CONFLICT(path) DO UPDATE SET rating = excluded.rating,
                             updated_at = excluded.updated_at",
                    )
                    .map_err(|e| format!("评分保存失败: {e}"))?;
                let mut n = 0;
                for p in paths {
                    n += stmt
                        .execute(rusqlite::params![p, v, now])
                        .map_err(|e| format!("评分保存失败: {e}"))?;
                }
                n
            }
            None => {
                let mut stmt = tx
                    .prepare_cached("DELETE FROM ratings WHERE path = ?1")
                    .map_err(|e| format!("评分保存失败: {e}"))?;
                let mut n = 0;
                for p in paths {
                    n += stmt
                        .execute(rusqlite::params![p])
                        .map_err(|e| format!("评分保存失败: {e}"))?;
                }
                n
            }
        };
        tx.commit().map_err(|e| format!("评分保存失败: {e}"))?;
        Ok(n)
    }

    /// 收藏集合：path 存在即为已收藏（与 ratings 分表，互不派生）
    pub fn all_favorites(&self) -> std::collections::HashSet<String> {
        let mut out = std::collections::HashSet::new();
        let Some(conn) = &self.conn else { return out };
        let Ok(db) = conn.lock() else { return out };
        let Ok(mut stmt) = db.prepare("SELECT path FROM favorites") else {
            return out;
        };
        let _ = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map(|rows| {
                for p in rows.flatten() {
                    out.insert(p);
                }
            });
        out
    }

    /// 批量收藏 / 取消收藏（单事务）。DB 不可用或写失败一律 Err，不静默成功。
    pub fn set_favorites_batch(
        &self,
        paths: &[String],
        favorite: bool,
    ) -> Result<usize, String> {
        let Some(conn) = &self.conn else {
            return Err("评分数据库不可用，收藏未保存".into());
        };
        let mut db = conn
            .lock()
            .map_err(|_| "评分数据库被占用，收藏未保存".to_string())?;
        let tx = db
            .transaction()
            .map_err(|e| format!("收藏保存失败: {e}"))?;
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let n = if favorite {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT OR IGNORE INTO favorites (path, created_at) VALUES (?1, ?2)",
                )
                .map_err(|e| format!("收藏保存失败: {e}"))?;
            let mut n = 0;
            for p in paths {
                n += stmt
                    .execute(rusqlite::params![p, now])
                    .map_err(|e| format!("收藏保存失败: {e}"))?;
            }
            n
        } else {
            let mut stmt = tx
                .prepare_cached("DELETE FROM favorites WHERE path = ?1")
                .map_err(|e| format!("收藏保存失败: {e}"))?;
            let mut n = 0;
            for p in paths {
                n += stmt
                    .execute(rusqlite::params![p])
                    .map_err(|e| format!("收藏保存失败: {e}"))?;
            }
            n
        };
        tx.commit().map_err(|e| format!("收藏保存失败: {e}"))?;
        Ok(n)
    }

    /// 一次性从文件内嵌 XMP 导入评分：只插不覆盖（用户已在本工具打的分优先）
    pub fn insert_ratings(&self, entries: &[(String, u8)]) -> Result<usize, String> {
        let Some(conn) = &self.conn else {
            return Err("评分数据库不可用，导入未保存".into());
        };
        let mut db = conn
            .lock()
            .map_err(|_| "评分数据库被占用，导入未保存".to_string())?;
        let tx = db.transaction().map_err(|e| format!("导入失败: {e}"))?;
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let mut n = 0;
        {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT OR IGNORE INTO ratings (path, rating, updated_at) VALUES (?1, ?2, ?3)",
                )
                .map_err(|e| format!("导入失败: {e}"))?;
            for (p, r) in entries {
                n += stmt
                    .execute(rusqlite::params![p, r, now])
                    .map_err(|e| format!("导入失败: {e}"))?;
            }
        }
        tx.commit().map_err(|e| format!("导入失败: {e}"))?;
        Ok(n)
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
                path TEXT PRIMARY KEY, mtime INTEGER NOT NULL, shoot_time TEXT
            ) WITHOUT ROWID;
            CREATE TABLE IF NOT EXISTS ratings (
                path TEXT PRIMARY KEY, rating INTEGER NOT NULL CHECK (rating BETWEEN 1 AND 5),
                updated_at TEXT NOT NULL
            ) WITHOUT ROWID;
            CREATE TABLE IF NOT EXISTS favorites (
                path TEXT PRIMARY KEY, created_at TEXT NOT NULL
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
    fn times_and_ratings_roundtrip() {
        let (store, dir) = temp_store("rt");
        store.upsert_patches(&[
            TimePatch {
                path: "a.jpg".into(),
                mtime_secs: 100,
                shoot_time: Some("2026-01-01 10:00:00".into()),
            },
            TimePatch {
                path: "b.RW2".into(),
                mtime_secs: 200,
                shoot_time: Some("2026-02-02 11:00:00".into()),
            },
        ]);
        let times = store.load_times();
        assert_eq!(times.get("a.jpg"), Some(&(100, Some("2026-01-01 10:00:00".into()))));
        assert_eq!(times.get("b.RW2"), Some(&(200, Some("2026-02-02 11:00:00".into()))));
        assert!(store.all_ratings().is_empty(), "初始无评分");

        // 评分与文件格式无关：RAW / 视频同样可评分
        store
            .set_ratings_batch(&["a.jpg".into(), "b.RW2".into(), "c.mp4".into()], Some(4))
            .unwrap();
        let ratings = store.all_ratings();
        assert_eq!(ratings.get("a.jpg"), Some(&4));
        assert_eq!(ratings.get("b.RW2"), Some(&4));
        assert_eq!(ratings.get("c.mp4"), Some(&4));

        // 清除 = 删行，不留 0 值
        store.set_ratings_batch(&["a.jpg".into()], None).unwrap();
        assert!(!store.all_ratings().contains_key("a.jpg"));

        // 一次性导入只插不覆盖：已存在的 b.RW2 保持 4 星
        store
            .insert_ratings(&[("b.RW2".into(), 2), ("d.dng".into(), 5)])
            .unwrap();
        let ratings = store.all_ratings();
        assert_eq!(ratings.get("b.RW2"), Some(&4));
        assert_eq!(ratings.get("d.dng"), Some(&5));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 本次改造最核心的断言：打分 / 清除评分绝不触碰图片文件。
    #[test]
    fn scoring_never_touches_photo() {
        let (store, dir) = temp_store("no-touch");
        let jpg = dir.join("a.jpg");
        std::fs::write(&jpg, b"original-photo-bytes").unwrap();
        let before = std::fs::read(&jpg).unwrap();
        let mtime = std::fs::metadata(&jpg).unwrap().modified().unwrap();
        let path = jpg.to_string_lossy().to_string();

        store.set_ratings_batch(&[path.clone()], Some(4)).unwrap();
        assert_eq!(store.all_ratings().get(&path), Some(&4));
        assert_eq!(std::fs::read(&jpg).unwrap(), before, "内容零改动");
        assert_eq!(std::fs::metadata(&jpg).unwrap().len(), before.len() as u64, "大小零改动");
        assert_eq!(std::fs::metadata(&jpg).unwrap().modified().unwrap(), mtime, "mtime 零改动");

        store.set_ratings_batch(&[path.clone()], None).unwrap();
        assert!(!store.all_ratings().contains_key(&path));
        assert_eq!(std::fs::read(&jpg).unwrap(), before, "清除评分也不改文件");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn favorites_roundtrip() {
        let (store, dir) = temp_store("fav");
        assert!(store.all_favorites().is_empty(), "初始无收藏");
        store
            .set_favorites_batch(&["a.jpg".into(), "b.RW2".into()], true)
            .unwrap();
        let fav = store.all_favorites();
        assert_eq!(fav.len(), 2);
        assert!(fav.contains("a.jpg") && fav.contains("b.RW2"));
        // 重复收藏幂等
        store.set_favorites_batch(&["a.jpg".into()], true).unwrap();
        assert_eq!(store.all_favorites().len(), 2);
        // 取消 = 删行
        store.set_favorites_batch(&["a.jpg".into()], false).unwrap();
        let fav = store.all_favorites();
        assert!(!fav.contains("a.jpg"));
        assert!(fav.contains("b.RW2"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 收藏同样绝不触碰图片文件（硬规则 #13）
    #[test]
    fn favorite_never_touches_photo() {
        let (store, dir) = temp_store("fav-no-touch");
        let jpg = dir.join("a.jpg");
        std::fs::write(&jpg, b"original-photo-bytes").unwrap();
        let before = std::fs::read(&jpg).unwrap();
        let mtime = std::fs::metadata(&jpg).unwrap().modified().unwrap();
        let path = jpg.to_string_lossy().to_string();

        store.set_favorites_batch(&[path.clone()], true).unwrap();
        assert!(store.all_favorites().contains(&path));
        assert_eq!(std::fs::read(&jpg).unwrap(), before, "内容零改动");
        assert_eq!(std::fs::metadata(&jpg).unwrap().modified().unwrap(), mtime, "mtime 零改动");

        store.set_favorites_batch(&[path.clone()], false).unwrap();
        assert!(!store.all_favorites().contains(&path));
        assert_eq!(std::fs::read(&jpg).unwrap(), before, "取消收藏也不改文件");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn degraded_mode_reports_error() {
        let store = MetaStore { conn: None };
        assert!(!store.is_available());
        assert!(store.set_ratings_batch(&["x".into()], Some(3)).is_err());
        assert!(store.insert_ratings(&[("x".into(), 3)]).is_err());
        assert!(store.set_favorites_batch(&["x".into()], true).is_err());
        store.upsert_patches(&[]);
        assert!(store.load_times().is_empty());
        assert!(store.all_ratings().is_empty());
        assert!(store.all_favorites().is_empty());
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
