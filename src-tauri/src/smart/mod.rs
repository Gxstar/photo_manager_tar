//! 智能相册独立模块（docs/开发方案.md §17.6、§20 第一批）。
//!
//! 本批只做四件事，全部是真实能力，不含任何假数据：
//! - 独立的 `smart.db`（与 `gallery.db` 物理隔离，删掉即回到无 AI 状态）；
//! - 模型目录的**真实就绪检测**（门禁只看文件是否存在且非空，与来源无关）；
//! - 地点相册 / GPS 点位的读写接口（供第二批扫描入库）；
//! - 打开模型目录。
//!
//! **明确未实现**：ONNX 推理、模型下载、SHA-256 校验、GPS EXIF 解析、地图瓦片。
//! GPS 解析规则先用 `tools/gps_coverage.py` 在真实媒体库上验证后再固化到 Rust。

mod gps;

use crate::state::AppState;
use rusqlite::{params, Connection};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tauri::{ipc::Channel, State};
use walkdir::WalkDir;

fn base_dir() -> PathBuf {
    std::env::var("LOCALAPPDATA")
        .map(|d| PathBuf::from(d).join("com.gengx.develop"))
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// 模型目录：%LOCALAPPDATA%\com.gengx.develop\models
pub fn models_dir() -> PathBuf {
    base_dir().join("models")
}

fn db_path() -> PathBuf {
    base_dir().join("smart.db")
}

/// 可直接在系统里打开、有原生缩略图的格式（JPG / HEIC / AVIF …）；
/// 其余（rw2 / dng / nef / cr2 / arw / orf）视为需要兜底的 RAW。
fn is_viewable_path(path: &str) -> bool {
    matches!(
        Path::new(path)
            .extension()
            .and_then(|x| x.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "jpg" | "jpeg" | "png" | "webp" | "heic" | "heif" | "avif" | "tif" | "tiff"
    )
}

/// 扫描白名单：只有这些扩展名才算「照片」（RAW 也在内，与 JPG 并列，各算一张）。
fn is_shot_file(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|x| x.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "jpg" | "jpeg" | "tif" | "tiff" | "rw2" | "dng" | "heic" | "avif" | "png" | "webp"
    )
}

/// 重扫 GPS 时按簇名保留的逆地理编码结果，避免重新请求天地图、重复消耗配额。
#[derive(Debug, Clone)]
struct PlaceGeo {
    province: String,
    city: String,
    county: String,
    address: String,
    geocoded: bool,
}

/// 重扫 GPS 时插入地点簇的语句；列与占位符必须严格对应，见
/// `insert_place_sql_column_count_matches` 单测（避免运行期才发现 12 values for 11 columns）。
const INSERT_PLACE_SQL: &str = "INSERT INTO albums (kind, name, cover_path, lat, lon,
        province, city, county, address, geocoded, created_at)
 VALUES ('place', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)";

fn load_place_geo(db: &Connection) -> HashMap<String, PlaceGeo> {
    let mut out = HashMap::new();
    let Ok(mut stmt) = db.prepare(
        "SELECT name, COALESCE(province,''), COALESCE(city,''), COALESCE(county,''),
                COALESCE(address,''), COALESCE(geocoded,0)
         FROM albums WHERE kind = 'place'
           AND (COALESCE(geocoded,0) = 1
                OR COALESCE(province,'') <> '' OR COALESCE(city,'') <> ''
                OR COALESCE(county,'') <> '' OR COALESCE(address,'') <> '')",
    ) else {
        return out;
    };
    let Ok(rows) = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, i64>(5)? != 0,
        ))
    }) else {
        return out;
    };
    for (name, province, city, county, address, geocoded) in rows.flatten() {
        out.insert(
            name,
            PlaceGeo {
                province,
                city,
                county,
                address,
                geocoded,
            },
        );
    }
    out
}

/// 固定清单：key / 显示名 / 体积(MB) / 许可 / 相对 models 目录的文件名。
/// 换模型版本 = 改这里 + 显式重建索引（§17.6 硬约束）。
const MANIFEST: &[(&str, &str, u32, &str, &[&str])] = &[
    (
        "cnclip",
        "中文语义搜图",
        280,
        "Apache-2.0",
        &[
            "cnclip-vit-b16/vision.onnx",
            "cnclip-vit-b16/text.onnx",
            "cnclip-vit-b16/tokenizer.json",
        ],
    ),
    (
        "faces",
        "人物相册",
        37,
        "Apache-2.0",
        &["faces/yunet.onnx", "faces/sface.onnx"],
    ),
];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelStatus {
    pub key: String,
    pub name: String,
    pub size_mb: u32,
    pub license: String,
    pub ready: bool,
    /// 已放入目录 / 未下载
    pub source: String,
    /// 缺失的文件（相对路径），用于「重新检测」时说明原因
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaceRow {
    pub id: i64,
    pub name: String,
    pub cover_path: Option<String>,
    pub count: i64,
    /// 簇中心（WGS-84），供地图打点
    pub lat: f64,
    pub lon: f64,
    /// 天地图逆地理编码结果；未解析时为空串
    pub province: String,
    pub city: String,
    pub county: String,
    /// 详细地址（formatted_address）；簇中心级，供图片详情显示
    pub address: String,
    /// 是否已成功请求过逆地理编码（即使详细地址为空也不重跑）
    pub geocoded: bool,
}

/// 单张照片的拍摄地点（图库信息栏用），来自它所属的 GPS 簇。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhotoLocation {
    pub province: String,
    pub city: String,
    pub county: String,
    pub address: String,
    pub lat: f64,
    pub lon: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanSummary {
    pub scanned: usize,
    pub with_gps: usize,
    pub places: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeocodeSummary {
    pub total: usize,
    pub ok: usize,
    pub failed: usize,
    pub message: String,
}

/// 解析进度（经 Channel 推送）。只含计数与坐标，绝不含 key / 路径。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeocodeProgress {
    pub done: usize,
    pub total: usize,
    pub ok: usize,
    pub failed: usize,
    pub current: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OnlineSettings {
    /// 只回传「是否已设置」，key 本身永不出后端
    pub key_set: bool,
    /// province | city | county
    pub level: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpsPoint {
    pub path: String,
    pub lat: f64,
    pub lon: f64,
    pub shoot_time: Option<String>,
}

pub struct Smart {
    conn: Option<Mutex<Connection>>,
}

impl Smart {
    pub fn open() -> Self {
        let _ = std::fs::create_dir_all(base_dir());
        let _ = std::fs::create_dir_all(models_dir());
        let conn = Connection::open(db_path())
            .and_then(|c| {
                c.execute_batch(
                    "CREATE TABLE IF NOT EXISTS gps_points (
                        path       TEXT PRIMARY KEY,
                        lat        REAL NOT NULL,
                        lon        REAL NOT NULL,
                        shoot_time TEXT,
                        place_id   INTEGER
                    ) WITHOUT ROWID;
                    CREATE TABLE IF NOT EXISTS albums (
                        id          INTEGER PRIMARY KEY AUTOINCREMENT,
                        kind        TEXT NOT NULL,
                        name        TEXT NOT NULL,
                        cover_path  TEXT,
                        lat         REAL,
                        lon         REAL,
                        province    TEXT,
                        city        TEXT,
                        county      TEXT,
                        address     TEXT,
                        geocoded    INTEGER,
                        created_at  TEXT NOT NULL
                    );
                    CREATE TABLE IF NOT EXISTS album_members (
                        album_id   INTEGER NOT NULL,
                        path       TEXT NOT NULL,
                        confidence REAL,
                        source     TEXT NOT NULL,
                        PRIMARY KEY (album_id, path)
                    );
                    CREATE TABLE IF NOT EXISTS model_sources (
                        key        TEXT PRIMARY KEY,
                        source     TEXT NOT NULL,
                        updated_at TEXT NOT NULL
                    ) WITHOUT ROWID;
                    CREATE TABLE IF NOT EXISTS settings (
                        key   TEXT PRIMARY KEY,
                        value TEXT NOT NULL
                    ) WITHOUT ROWID;",
                )?;
                // 第一批已建过 albums 表时补列；重复执行会报 duplicate column，忽略即可。
                let _ = c.execute("ALTER TABLE albums ADD COLUMN lat REAL", []);
                let _ = c.execute("ALTER TABLE albums ADD COLUMN lon REAL", []);
                let _ = c.execute("ALTER TABLE albums ADD COLUMN province TEXT", []);
                let _ = c.execute("ALTER TABLE albums ADD COLUMN city TEXT", []);
                let _ = c.execute("ALTER TABLE albums ADD COLUMN county TEXT", []);
                let _ = c.execute("ALTER TABLE albums ADD COLUMN address TEXT", []);
                let _ = c.execute("ALTER TABLE albums ADD COLUMN geocoded INTEGER", []);
                Ok(c)
            })
            .map(Mutex::new)
            .map_err(|e: rusqlite::Error| {
                eprintln!("[smart] smart.db 打开失败，降级为纯内存: {e}");
            });
        let smart = Self { conn: conn.ok() };
        smart.migrate_restore_shot_pairs();
        smart
    }

    /// 修复迁移：把「同名 JPG / RAW 是同一张照片」的错误合并恢复成两张独立文件（docs §32）。
    ///
    /// §31 曾把 `P1000001.JPG` 与 `P1000001.RW2` 合并成一条成员，用户确认这个方向不对：
    /// RAW 与 JPG 是**两个文件**，都要在相册里出现，靠缩略图上的 RAW 角标区分。
    /// 这里按「同目录 + 同主名」把被删掉的另一半补回来（只增不删，坐标与 JPG 相同，
    /// 因为本来就是同一次快门），不需要重扫 4 分钟。
    ///
    /// 幂等：完成后写 `smart_schema=3`。
    fn migrate_restore_shot_pairs(&self) {
        if self.get_setting("smart_schema").as_deref() == Some("3") {
            return;
        }
        let Some(conn) = &self.conn else { return };
        let Ok(mut db) = conn.lock() else { return };
        let Ok(tx) = db.transaction() else { return };

        let rows: Vec<(i64, String)> = {
            let Ok(mut stmt) = tx.prepare(
                "SELECT m.album_id, m.path FROM album_members m
                 JOIN albums a ON a.id = m.album_id WHERE a.kind = 'place'",
            ) else {
                return;
            };
            match stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))) {
                Ok(it) => it.flatten().collect(),
                Err(_) => return,
            }
        };

        // 按目录归并，每个目录只 read_dir 一次（HDD 上这点很重要）
        let mut by_dir: HashMap<String, Vec<(i64, String)>> = HashMap::new();
        let mut existing: HashSet<(i64, String)> = HashSet::new();
        let mut existing_paths: HashSet<String> = HashSet::new();
        for (album_id, path) in rows {
            let dir = Path::new(&path)
                .parent()
                .map(|d| d.to_string_lossy().to_string())
                .unwrap_or_default();
            existing.insert((album_id, path.clone()));
            existing_paths.insert(path.clone());
            by_dir.entry(dir).or_default().push((album_id, path));
        }

        let mut added = 0usize;
        for (dir, items) in by_dir {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            // 同目录内按主名建索引：P1000001 → [.JPG, .RW2]
            let mut by_stem: HashMap<String, Vec<String>> = HashMap::new();
            for e in rd.flatten() {
                let p = e.path();
                if !p.is_file() || !is_shot_file(&p) {
                    continue;
                }
                let stem = p
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                by_stem
                    .entry(stem)
                    .or_default()
                    .push(p.to_string_lossy().to_string());
            }
            for (album_id, path) in items {
                let stem = Path::new(&path)
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                let Some(siblings) = by_stem.get(&stem) else {
                    continue;
                };
                for sib in siblings {
                    if existing.contains(&(album_id, sib.clone())) {
                        continue;
                    }
                    // 已经在别的相册里就不再复制：只补「被删掉的另一半」，
                    // 不在相册间搬照片（边界坐标可能让同名两张落进相邻格子）
                    if existing_paths.contains(sib) {
                        continue;
                    }
                    if tx
                        .execute(
                            "INSERT OR IGNORE INTO album_members (album_id, path, confidence, source)
                             VALUES (?1, ?2, 1.0, 'gps')",
                            params![album_id, sib],
                        )
                        .is_ok()
                    {
                        existing.insert((album_id, sib.clone()));
                        existing_paths.insert(sib.clone());
                        added += 1;
                    }
                }
            }
        }

        let _ = tx.execute(
            "INSERT INTO settings (key, value) VALUES ('smart_schema', '3')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [],
        );
        if tx.commit().is_ok() && added > 0 {
            eprintln!("[smart] 已恢复被合并掉的同名 RAW/JPG 成员 {added} 条（docs §32）");
        }
    }

    /// 地点相册：只从库里读，没有就是空 —— 绝不生成假相册。
    pub fn places(&self) -> Vec<PlaceRow> {
        let Some(conn) = &self.conn else { return Vec::new() };
        let Ok(db) = conn.lock() else { return Vec::new() };
        let Ok(mut stmt) = db.prepare(
            "SELECT a.id, a.name, a.cover_path,
                    (SELECT COUNT(*) FROM album_members m WHERE m.album_id = a.id),
                    COALESCE(a.lat, 0), COALESCE(a.lon, 0),
                    COALESCE(a.province, ''), COALESCE(a.city, ''), COALESCE(a.county, ''),
                    COALESCE(a.address, ''), COALESCE(a.geocoded, 0)
             FROM albums a WHERE a.kind = 'place' ORDER BY a.id",
        ) else {
            return Vec::new();
        };
        let rows = stmt.query_map([], |r| {
            Ok(PlaceRow {
                id: r.get(0)?,
                name: r.get(1)?,
                cover_path: r.get(2)?,
                count: r.get(3)?,
                lat: r.get(4)?,
                lon: r.get(5)?,
                province: r.get(6)?,
                city: r.get(7)?,
                county: r.get(8)?,
                address: r.get(9)?,
                geocoded: r.get::<_, i64>(10)? != 0,
            })
        });
        match rows {
            Ok(it) => it.flatten().collect(),
            Err(e) => {
                eprintln!("[smart] 读取地点相册失败: {e}");
                Vec::new()
            }
        }
    }

    /// 供第二批 GPS 扫描入库；同一 path 重复写入是幂等 upsert。
    pub fn ingest_gps(&self, points: &[GpsPoint]) -> usize {
        let Some(conn) = &self.conn else { return 0 };
        let Ok(mut db) = conn.lock() else { return 0 };
        let Ok(tx) = db.transaction() else { return 0 };
        let mut n = 0;
        {
            let Ok(mut stmt) = tx.prepare_cached(
                "INSERT INTO gps_points (path, lat, lon, shoot_time) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(path) DO UPDATE SET lat = excluded.lat,
                     lon = excluded.lon, shoot_time = excluded.shoot_time",
            ) else {
                return 0;
            };
            for p in points {
                n += stmt
                    .execute(params![p.path, p.lat, p.lon, p.shoot_time])
                    .unwrap_or(0);
            }
        }
        let _ = tx.commit();
        n
    }

    /// 扫描媒体库 → GPS 点位 → 坐标聚类 → 生成地点相册。
    /// **只读照片文件**，绝不写入 / 移动 / 删除；重建时保留用户手动指定的封面。
    pub fn scan_gps(
        &self,
        root: &str,
        original_dir: &str,
        favorites: &HashSet<String>,
        ratings: &HashMap<String, u8>,
        rejects: &HashSet<String>,
    ) -> Result<ScanSummary, String> {
        let base = Path::new(root).join(original_dir);
        if !base.is_dir() {
            return Err(format!("媒体库目录不可用：{}", base.display()));
        }
        let mut points: Vec<(String, f64, f64, i64)> = Vec::new();
        let mut scanned = 0usize;
        for entry in WalkDir::new(&base).into_iter().filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if !is_shot_file(path) {
                continue;
            }
            scanned += 1;
            if let Some((lat, lon)) = gps::read_gps(path) {
                let mtime = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                points.push((path.to_string_lossy().to_string(), lat, lon, mtime));
            }
        }
        // 同一次快门的 JPG 与 RAW 是**两张独立文件**，各入一条（docs §32）。
        // 曾经按「一次快门」合并过，导致相册里少了 RAW；现在靠缩略图上的 RAW 角标区分。
        let with_gps = points.len();

        let Some(conn) = &self.conn else {
            return Err("smart.db 不可用".into());
        };
        let mut db = conn.lock().map_err(|_| "smart.db 被占用".to_string())?;
        let tx = db.transaction().map_err(|e| e.to_string())?;

        // 已解析的地名按簇名保留：重扫 GPS 不重复请求天地图（配额硬约束 §25）
        let geo_cache = load_place_geo(&tx);

        // 用户手动指定的封面按相册名保留，重建不覆盖用户结论（§19.4）
        let mut user_covers: HashMap<String, String> = HashMap::new();
        {
            let mut stmt = tx
                .prepare("SELECT name, cover_path FROM albums WHERE kind = 'place' AND cover_path IS NOT NULL")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(|e| e.to_string())?;
            for r in rows.flatten() {
                user_covers.insert(r.0, r.1);
            }
        }
        tx.execute(
            "DELETE FROM album_members WHERE album_id IN (SELECT id FROM albums WHERE kind = 'place')",
            [],
        )
        .map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM albums WHERE kind = 'place'", [])
            .map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM gps_points", [])
            .map_err(|e| e.to_string())?;
        {
            let mut stmt = tx
                .prepare_cached("INSERT OR REPLACE INTO gps_points (path, lat, lon) VALUES (?1, ?2, ?3)")
                .map_err(|e| e.to_string())?;
            for (path, lat, lon, _) in &points {
                stmt.execute(params![path, lat, lon])
                    .map_err(|e| e.to_string())?;
            }
        }

        // 0.01° 网格聚类（约 1km）：确定性、无第三方聚类库、结果稳定。
        let mut groups: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
        for (i, (_, lat, lon, _)) in points.iter().enumerate() {
            groups
                .entry(((lat / 0.01).round() as i64, (lon / 0.01).round() as i64))
                .or_default()
                .push(i);
        }
        let now = chrono::Local::now()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let mut places = 0usize;
        for (_key, idxs) in groups {
            if idxs.is_empty() {
                continue;
            }
            let lat = idxs.iter().map(|&i| points[i].1).sum::<f64>() / idxs.len() as f64;
            let lon = idxs.iter().map(|&i| points[i].2).sum::<f64>() / idxs.len() as f64;
            let name = format!(
                "{:.3}°{} {:.3}°{}",
                lat.abs(),
                if lat >= 0.0 { "N" } else { "S" },
                lon.abs(),
                if lon >= 0.0 { "E" } else { "W" }
            );
            // 代表图优先级：用户指定 > 收藏 > ≥4★ > 最新；排除废片
            let mut best: Option<(i64, i64, String)> = None;
            for &i in &idxs {
                let (path, _, _, mt) = &points[i];
                if rejects.contains(path) {
                    continue;
                }
                let pri = if favorites.contains(path) {
                    3
                } else if ratings.get(path).copied().unwrap_or(0) >= 4 {
                    2
                } else {
                    1
                };
                if best
                    .as_ref()
                    .map(|(p, m, _)| (pri, *mt) > (*p, *m))
                    .unwrap_or(true)
                {
                    best = Some((pri, *mt, path.clone()));
                }
            }
            let mut cover = user_covers
                .get(&name)
                .cloned()
                .or_else(|| best.map(|(_, _, p)| p));
            // 全是 RAW 的簇里优先挑一张可查看格式当封面（缩略图更稳、点开更顺）
            if cover.as_deref().map(|p| !is_viewable_path(p)).unwrap_or(true) {
                cover = idxs
                    .iter()
                    .map(|&i| &points[i].0)
                    .find(|p| is_viewable_path(p))
                    .cloned()
                    .or(cover);
            }
            let geo = geo_cache.get(&name);
            tx.execute(
                INSERT_PLACE_SQL,
                params![
                    name.as_str(),
                    cover,
                    lat,
                    lon,
                    geo.map(|g| g.province.as_str()).unwrap_or(""),
                    geo.map(|g| g.city.as_str()).unwrap_or(""),
                    geo.map(|g| g.county.as_str()).unwrap_or(""),
                    geo.map(|g| g.address.as_str()).unwrap_or(""),
                    geo.is_some_and(|g| g.geocoded),
                    now
                ],
            )
            .map_err(|e| e.to_string())?;
            let id = tx.last_insert_rowid();
            for &i in &idxs {
                tx.execute(
                    "INSERT OR IGNORE INTO album_members (album_id, path, confidence, source)
                     VALUES (?1, ?2, 1.0, 'gps')",
                    params![id, points[i].0],
                )
                .map_err(|e| e.to_string())?;
            }
            places += 1;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(ScanSummary {
            scanned,
            with_gps,
            places,
        })
    }

    /// 相册成员路径（供浏览与手动设封面），按路径排序。
    pub fn album_photos(&self, id: i64) -> Vec<String> {
        let Some(conn) = &self.conn else {
            return Vec::new();
        };
        let Ok(db) = conn.lock() else {
            return Vec::new();
        };
        let Ok(mut stmt) =
            db.prepare("SELECT path FROM album_members WHERE album_id = ?1 ORDER BY path")
        else {
            return Vec::new();
        };
        match stmt.query_map(params![id], |r| r.get::<_, String>(0)) {
            // 同上：不做缩略图路径替换，保留真实成员路径
            Ok(rows) => rows.flatten().collect(),
            Err(e) => {
                eprintln!("[smart] 读取相册成员失败: {e}");
                Vec::new()
            }
        }
    }

    pub fn rename_album(&self, id: i64, name: &str) -> Result<(), String> {
        let Some(conn) = &self.conn else {
            return Err("smart.db 不可用".into());
        };
        let db = conn.lock().map_err(|_| "smart.db 被占用".to_string())?;
        let n = db
            .execute(
                "UPDATE albums SET name = ?1 WHERE id = ?2 AND kind = 'place'",
                params![name.trim(), id],
            )
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("地点相册不存在".into());
        }
        Ok(())
    }

    pub fn set_cover(&self, id: i64, path: &str) -> Result<(), String> {
        let Some(conn) = &self.conn else {
            return Err("smart.db 不可用".into());
        };
        let db = conn.lock().map_err(|_| "smart.db 被占用".to_string())?;
        let n = db
            .execute(
                "UPDATE albums SET cover_path = ?1 WHERE id = ?2 AND kind = 'place'",
                params![path, id],
            )
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("地点相册不存在".into());
        }
        Ok(())
    }

    /// 合并相册的成员：按多个 GPS 簇 id 取并集去重（按省 / 市 / 县归类后使用）。
    pub fn group_photos(&self, ids: &[i64]) -> Vec<String> {
        if ids.is_empty() {
            return Vec::new();
        }
        let Some(conn) = &self.conn else {
            return Vec::new();
        };
        let Ok(db) = conn.lock() else {
            return Vec::new();
        };
        // ids 全是 i64，无注入风险
        let marks = vec!["?"; ids.len()].join(",");
        let sql = format!(
            "SELECT DISTINCT path FROM album_members WHERE album_id IN ({marks}) ORDER BY path"
        );
        let Ok(mut stmt) = db.prepare(&sql) else {
            return Vec::new();
        };
        match stmt.query_map(rusqlite::params_from_iter(ids.iter()), |r| {
            r.get::<_, String>(0)
        }) {
            // 返回**原始成员路径**：套 display_thumb_path 会把同名的 JPG 与 RW2 映射成
            // 同一个字符串，前端就会出现两个内容相同的格子、两个「封面」角标（docs §31）。
            Ok(rows) => rows.flatten().collect(),
            Err(e) => {
                eprintln!("[smart] 读取分组相册成员失败: {e}");
                Vec::new()
            }
        }
    }

    /// 单张照片的拍摄地点（它所属的 GPS 簇），供图库信息栏显示详细地址。
    /// JPG+RAW 同拍同名时 GPS 常只写在 RAW 上，故 JPG 查不到时回退同名 RAW。
    pub fn photo_location(&self, path: &str) -> Option<PhotoLocation> {
        if let Some(loc) = self.query_photo_location(path) {
            return Some(loc);
        }
        let p = Path::new(path);
        let ext = p
            .extension()
            .and_then(|x| x.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !matches!(ext.as_str(), "jpg" | "jpeg") {
            return None;
        }
        for e in [
            "RW2", "rw2", "DNG", "dng", "NEF", "nef", "CR2", "cr2", "ARW", "arw",
        ] {
            let candidate = p.with_extension(e).to_string_lossy().to_string();
            if let Some(loc) = self.query_photo_location(&candidate) {
                return Some(loc);
            }
        }
        None
    }

    fn query_photo_location(&self, path: &str) -> Option<PhotoLocation> {
        let db = self.conn.as_ref()?.lock().ok()?;
        db.query_row(
            "SELECT COALESCE(a.province,''), COALESCE(a.city,''), COALESCE(a.county,''),
                    COALESCE(a.address,''), COALESCE(a.lat,0), COALESCE(a.lon,0)
             FROM album_members m JOIN albums a ON a.id = m.album_id
             WHERE m.path = ?1 AND a.kind = 'place' LIMIT 1",
            params![path],
            |r| {
                Ok(PhotoLocation {
                    province: r.get(0)?,
                    city: r.get(1)?,
                    county: r.get(2)?,
                    address: r.get(3)?,
                    lat: r.get(4)?,
                    lon: r.get(5)?,
                })
            },
        )
        .ok()
    }

    /// 分级归类后相册的用户自定义名 / 封面，复用 settings 表（不新增表）。
    /// key 形如 `group:alias:city:成都市` / `group:cover:city:成都市`。
    pub fn group_overrides(&self) -> HashMap<String, String> {
        let Some(db) = self.conn.as_ref().and_then(|c| c.lock().ok()) else {
            return HashMap::new();
        };
        let Ok(mut stmt) = db.prepare("SELECT key, value FROM settings WHERE key LIKE 'group:%'")
        else {
            return HashMap::new();
        };
        match stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        }) {
            Ok(it) => it.flatten().collect(),
            Err(_) => HashMap::new(),
        }
    }

    pub fn set_group_override(&self, key: &str, value: &str) -> Result<(), String> {
        if !key.starts_with("group:alias:") && !key.starts_with("group:cover:") {
            return Err("非法的分组设置键".into());
        }
        self.set_setting(key, value)
    }

    pub fn get_setting(&self, key: &str) -> Option<String> {
        let db = self.conn.as_ref()?.lock().ok()?;
        db.query_row("SELECT value FROM settings WHERE key = ?1", params![key], |r| {
            r.get::<_, String>(0)
        })
        .ok()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), String> {
        let Some(conn) = &self.conn else {
            return Err("smart.db 不可用".into());
        };
        let db = conn.lock().map_err(|_| "smart.db 被占用".to_string())?;
        if value.is_empty() {
            db.execute("DELETE FROM settings WHERE key = ?1", params![key])
                .map_err(|e| e.to_string())?;
        } else {
            db.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// 按簇中心逐个请求天地图逆地理编码，结果缓存进 albums；已解析的跳过。
    /// **只发送经纬度**，不发送照片 / 路径 / 任何库内信息；失败不中断其它相册。
    pub fn geocode_places<F>(&self, key: &str, mut progress: F) -> Result<GeocodeSummary, String>
    where
        F: FnMut(usize, usize, usize, usize, &str),
    {
        let Some(conn) = &self.conn else {
            return Err("smart.db 不可用".into());
        };
        let targets: Vec<(i64, f64, f64)> = {
            let db = conn.lock().map_err(|_| "smart.db 被占用".to_string())?;
            let mut stmt = db
                .prepare(
                    "SELECT id, COALESCE(lat, 0), COALESCE(lon, 0) FROM albums
                     WHERE kind = 'place' AND COALESCE(geocoded, 0) = 0",
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .map_err(|e| e.to_string())?;
            rows.flatten().collect()
        };
        let (mut ok, mut failed) = (0, 0);
        let mut message = String::new();
        let total = targets.len();
        progress(0, total, 0, 0, "准备中");
        for (idx, (id, lat, lon)) in targets.iter().enumerate() {
            let mut attempt = 0u32;
            loop {
                match geocode_one(*lat, *lon, key) {
                    Ok(g) => {
                        let db = conn.lock().map_err(|_| "smart.db 被占用".to_string())?;
                        db.execute(
                            "UPDATE albums SET province = ?1, city = ?2, county = ?3,
                                    address = ?4, geocoded = 1 WHERE id = ?5",
                            params![g.province, g.city, g.county, g.address, id],
                        )
                        .map_err(|e| e.to_string())?;
                        ok += 1;
                        break;
                    }
                    // 429 是单 key 的频率限制，指数退避后重试；不把原始错误（含 URL/tk）外泄
                    Err(GeoErr::RateLimited) if attempt < 3 => {
                        attempt += 1;
                        std::thread::sleep(std::time::Duration::from_millis(700 << (attempt - 1)));
                    }
                    Err(e) => {
                        failed += 1;
                        if message.is_empty() {
                            message = e.text();
                        }
                        break;
                    }
                }
            }
            let current = format!("{:.3}°, {:.3}°", lat, lon);
            progress(idx + 1, total, ok, failed, &current);
            // 个人 key 约 10 QPS 上限：350ms ≈ 2.8 QPS，留足余量避免瞬时超限
            std::thread::sleep(std::time::Duration::from_millis(350));
        }
        Ok(GeocodeSummary {
            total,
            ok,
            failed,
            message,
        })
    }
}

/// 门禁唯一依据：文件是否存在且非空。谁放进去的不重要（§17.15.7）。
#[tauri::command]
pub fn smart_model_status() -> Vec<ModelStatus> {
    let root = models_dir();
    MANIFEST
        .iter()
        .map(|(key, name, size, license, files)| {
            let missing: Vec<String> = files
                .iter()
                .filter(|f| {
                    std::fs::metadata(root.join(f))
                        .map(|m| m.len() == 0)
                        .unwrap_or(true)
                })
                .map(|f| (*f).to_string())
                .collect();
            ModelStatus {
                key: (*key).to_string(),
                name: (*name).to_string(),
                size_mb: *size,
                license: (*license).to_string(),
                ready: missing.is_empty(),
                source: if missing.is_empty() {
                    "已放入目录".to_string()
                } else {
                    "未下载".to_string()
                },
                missing,
            }
        })
        .collect()
}

#[tauri::command]
pub fn smart_places(state: State<'_, AppState>) -> Vec<PlaceRow> {
    state.smart.places()
}

#[tauri::command]
pub fn smart_ingest_gps(state: State<'_, AppState>, points: Vec<GpsPoint>) -> usize {
    state.smart.ingest_gps(&points)
}

#[tauri::command]
pub fn smart_db_path() -> String {
    db_path().to_string_lossy().to_string()
}

/// 打开模型目录：复用 Windows 资源管理器，不引额外插件。
#[tauri::command]
pub fn smart_open_models_dir() -> Result<(), String> {
    std::process::Command::new("explorer")
        .arg(models_dir())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("打开模型目录失败: {e}"))
}

/// 扫描媒体库 GPS 并重建地点相册。放到 blocking 线程池，避免阻塞 UI。
#[tauri::command]
pub async fn smart_scan_gps(state: State<'_, AppState>) -> Result<ScanSummary, String> {
    let cfg = state.config.lock().unwrap().clone();
    let favorites = state.meta.all_favorites();
    let ratings = state.meta.all_ratings();
    let rejects = state.meta.all_rejects();
    let result = tauri::async_runtime::spawn_blocking(move || {
        Smart::open().scan_gps(
            &cfg.library_root,
            &cfg.original_dir,
            &favorites,
            &ratings,
            &rejects,
        )
    })
    .await
    .map_err(|e| e.to_string())?;
    result
}

#[tauri::command]
pub fn smart_rename_album(
    state: State<'_, AppState>,
    id: i64,
    name: String,
) -> Result<(), String> {
    state.smart.rename_album(id, &name)
}

#[tauri::command]
pub fn smart_set_cover(
    state: State<'_, AppState>,
    id: i64,
    path: String,
) -> Result<(), String> {
    state.smart.set_cover(id, &path)
}

#[tauri::command]
pub fn smart_album_photos(state: State<'_, AppState>, id: i64) -> Vec<String> {
    state.smart.album_photos(id)
}

/// 按省 / 市 / 县合并后的相册成员（多个 GPS 簇取并集）。
#[tauri::command]
pub fn smart_group_photos(state: State<'_, AppState>, ids: Vec<i64>) -> Vec<String> {
    state.smart.group_photos(&ids)
}

/// 图库信息栏：单张照片的拍摄地点与详细地址。
#[tauri::command]
pub fn smart_photo_location(
    state: State<'_, AppState>,
    path: String,
) -> Option<PhotoLocation> {
    state.smart.photo_location(&path)
}

#[tauri::command]
pub fn smart_group_overrides(state: State<'_, AppState>) -> HashMap<String, String> {
    state.smart.group_overrides()
}

#[tauri::command]
pub fn smart_set_group_override(
    state: State<'_, AppState>,
    key: String,
    value: String,
) -> Result<(), String> {
    state.smart.set_group_override(&key, &value)
}

/// 逆地理编码结果：三级行政区划 + 详细地址。
#[derive(Debug)]
struct GeoInfo {
    province: String,
    city: String,
    county: String,
    address: String,
}

/// 对外只保留可读短文本，**绝不带 URL / tk**（ureq 的 Error Display 含完整 URL）。
#[derive(Debug)]
enum GeoErr {
    RateLimited,
    Other(String),
}
impl GeoErr {
    fn text(self) -> String {
        match self {
            GeoErr::RateLimited => {
                "天地图请求过于频繁（HTTP 429），重试后仍失败；稍后再点一次即可补齐".into()
            }
            GeoErr::Other(s) => s,
        }
    }
}

/// 天地图逆地理编码（官方 WEB 服务 API）：GET + postStr JSON + tk。
/// 只发 lon/lat/ver；8 秒超时；429 交调用方指数退避重试。
fn geocode_one(lat: f64, lon: f64, key: &str) -> Result<GeoInfo, GeoErr> {
    let post = format!("{{\"lon\":{lon},\"lat\":{lat},\"ver\":1}}");
    let encoded =
        percent_encoding::utf8_percent_encode(&post, percent_encoding::NON_ALPHANUMERIC)
            .to_string();
    // url 含 tk，只允许用于发起请求，禁止进入日志 / 错误信息 / 前端提示。
    let url = format!(
        "http://api.tianditu.gov.cn/geocoder?postStr={encoded}&type=geocode&tk={key}"
    );
    let body = match ureq::get(&url)
        .timeout(std::time::Duration::from_secs(8))
        .call()
    {
        Ok(r) => r
            .into_string()
            .map_err(|_| GeoErr::Other("读取响应失败".into()))?,
        Err(ureq::Error::Status(429, _)) => return Err(GeoErr::RateLimited),
        Err(ureq::Error::Status(code, _)) => {
            return Err(GeoErr::Other(format!("天地图返回 HTTP {code}")))
        }
        Err(_) => return Err(GeoErr::Other("网络请求失败或超时".into())),
    };
    parse_geocode(&body)
}

fn parse_geocode(body: &str) -> Result<GeoInfo, GeoErr> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|_| GeoErr::Other("响应不是 JSON".into()))?;
    if v["status"].as_str() != Some("0") {
        return Err(GeoErr::Other(format!(
            "天地图返回：{}",
            v["msg"].as_str().unwrap_or("未知错误")
        )));
    }
    let a = &v["result"]["addressComponent"];
    let get = |k: &str| a[k].as_str().unwrap_or("").trim().to_string();
    let (mut province, mut city, county) = (get("province"), get("city"), get("county"));
    if province.is_empty() && city.is_empty() && county.is_empty() {
        return Err(GeoErr::Other("响应中没有行政区划信息".into()));
    }
    // 直辖市（北京 / 上海 / 天津 / 重庆）常把市名放在 province、city 留空。
    // 补齐后按「市」级别归类才不会回退成坐标；已入库的旧数据由前端 normAdmin 兜底，
    // 不需要重新解析、不消耗配额。
    if city.is_empty() && !province.is_empty() {
        city = province.clone();
    }
    if province.is_empty() && !city.is_empty() {
        province = city.clone();
    }
    // 官方 formatted_address 即详细地址；缺失时用 addressComponent 细粒度字段兜底。
    let formatted = v["result"]["formatted_address"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();
    let (road, detail, poi) = (get("road"), get("address"), get("poi"));
    let address = if !formatted.is_empty() {
        formatted
    } else {
        let mut parts: Vec<String> = Vec::new();
        for s in [&province, &city, &county, &road, &detail, &poi] {
            if !s.is_empty() && !parts.contains(s) {
                parts.push(s.clone());
            }
        }
        parts.join("")
    };
    Ok(GeoInfo {
        province,
        city,
        county,
        address,
    })
}

#[tauri::command]
pub fn smart_get_settings(state: State<'_, AppState>) -> OnlineSettings {
    OnlineSettings {
        key_set: state
            .smart
            .get_setting("tianditu_key")
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        level: state
            .smart
            .get_setting("place_level")
            .unwrap_or_else(|| "city".to_string()),
    }
}

#[tauri::command]
pub fn smart_set_tianditu_key(state: State<'_, AppState>, key: String) -> Result<(), String> {
    state.smart.set_setting("tianditu_key", key.trim())
}

#[tauri::command]
pub fn smart_set_place_level(state: State<'_, AppState>, level: String) -> Result<(), String> {
    let v = level.trim();
    if !matches!(v, "province" | "city" | "county") {
        return Err("显示级别只能是省 / 市 / 县".into());
    }
    state.smart.set_setting("place_level", v)
}

#[tauri::command]
pub async fn smart_geocode_places(
    state: State<'_, AppState>,
    on_progress: Channel<GeocodeProgress>,
) -> Result<GeocodeSummary, String> {
    let key = state
        .smart
        .get_setting("tianditu_key")
        .unwrap_or_default();
    if key.is_empty() {
        return Err("尚未填写天地图 key，请到设置页填写".into());
    }
    let result = tauri::async_runtime::spawn_blocking(move || {
        Smart::open().geocode_places(&key, |done, total, ok, failed, current| {
            let _ = on_progress.send(GeocodeProgress {
                done,
                total,
                ok,
                failed,
                current: current.to_string(),
            });
        })
    })
    .await
    .map_err(|e| e.to_string())?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 端到端只读照片 + 写 smart.db 索引。默认 ignore，
    /// 仅显式带 SDI_FIXTURE_ROOT 与 --ignored 时运行；索引可随时重建。
    #[test]
    #[ignore]
    fn fixture_scan_gps() {
        let Ok(root) = std::env::var("SDI_FIXTURE_ROOT") else {
            return;
        };
        if root.is_empty() {
            return;
        }
        let smart = Smart::open();
        let summary = smart
            .scan_gps(
                &root,
                "原图",
                &HashSet::new(),
                &HashMap::new(),
                &HashSet::new(),
            )
            .expect("扫描失败");
        eprintln!("[smart fixture] {summary:?}");
        let places = smart.places();
        let jpg_covers = places
            .iter()
            .filter(|p| {
                p.cover_path.as_deref().map(|s| {
                    let s = s.to_ascii_lowercase();
                    s.ends_with(".jpg") || s.ends_with(".jpeg")
                }).unwrap_or(false)
            })
            .count();
        eprintln!(
            "[smart fixture] places={} jpg_covers={} first={:?}",
            places.len(),
            jpg_covers,
            places
                .first()
                .map(|p| (&p.name, p.count, p.cover_path.as_deref()))
        );
    }

    /// 真实 smart.db：触发列迁移、成员路径能查到拍摄地点、JPG 同名 RAW 回退可用。
    #[test]
    fn fixture_photo_location_from_real_index() {
        let Ok(root) = std::env::var("SDI_FIXTURE_ROOT") else {
            return;
        };
        if root.is_empty() {
            return;
        }
        let smart = Smart::open(); // 同时验证 address / geocoded 的 ALTER 迁移
        let Some(conn) = &smart.conn else {
            return;
        };
        let paths: Vec<String> = {
            let db = conn.lock().unwrap();
            db.prepare("SELECT path FROM album_members LIMIT 200")
                .and_then(|mut s| {
                    s.query_map([], |r| r.get::<_, String>(0))
                        .map(|it| it.flatten().collect())
                })
                .unwrap_or_default()
        };
        if paths.is_empty() {
            return;
        }
        let hit = paths
            .iter()
            .filter(|p| smart.photo_location(p).is_some())
            .count();
        eprintln!("[smart fixture] photo_location 命中 {hit}/{}", paths.len());
        assert_eq!(hit, paths.len(), "GPS 成员路径必须都能查到拍摄地点");

        let jpg_hit = paths
            .iter()
            .filter(|p| {
                Path::new(p)
                    .extension()
                    .and_then(|x| x.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("rw2"))
            })
            .filter(|p| {
                let jpg = Path::new(p).with_extension("JPG");
                jpg.is_file()
                    && smart
                        .photo_location(jpg.to_string_lossy().as_ref())
                        .is_some()
            })
            .count();
        eprintln!("[smart fixture] JPG→同名 RAW 回退命中 {jpg_hit}");
    }

    /// INSERT 列数与占位符数必须匹配；此前多写一个 ?11 导致扫描运行期
    /// 报「12 values for 11 columns」，抽成常量后用单测守住。
    #[test]
    fn insert_place_sql_column_count_matches() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE albums (
                id INTEGER PRIMARY KEY AUTOINCREMENT, kind TEXT NOT NULL, name TEXT NOT NULL,
                cover_path TEXT, lat REAL, lon REAL, province TEXT, city TEXT,
                county TEXT, address TEXT, geocoded INTEGER, created_at TEXT NOT NULL
             );",
        )
        .unwrap();
        let n = c
            .execute(
                super::INSERT_PLACE_SQL,
                params![
                    "40.037N 94.804E",
                    Option::<String>::None,
                    40.037_f64,
                    94.804_f64,
                    "甘肃省",
                    "酒泉市",
                    "敦煌市",
                    "鸣沙山",
                    true,
                    "2026-11-07 20:00:00"
                ],
            )
            .expect("INSERT 应成功");
        assert_eq!(n, 1);
        let (city, addr, geo): (String, String, i64) = c
            .query_row("SELECT city, address, geocoded FROM albums", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(city, "酒泉市");
        assert_eq!(addr, "鸣沙山");
        assert_eq!(geo, 1);
    }

    /// 重扫 GPS 会 DELETE albums 重建：已解析点位必须按簇名保留，
    /// 否则每次重扫都要重新请求天地图、白耗配额（§25）。
    #[test]
    fn scan_preserves_geocoded_places() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE albums (
                id INTEGER PRIMARY KEY, kind TEXT, name TEXT, cover_path TEXT,
                province TEXT, city TEXT, county TEXT, address TEXT, geocoded INTEGER
             );
             INSERT INTO albums (kind,name,province,city,county,address,geocoded)
               VALUES ('place','40.037N 94.804E','甘肃省','酒泉市','敦煌市','甘肃省酒泉市敦煌市鸣沙山',1);
             INSERT INTO albums (kind,name,province,city,county,address,geocoded)
               VALUES ('place','旧版只有三级','四川省','成都市','双流区','',0);
             INSERT INTO albums (kind,name,province,city,county,address,geocoded)
               VALUES ('place','未解析点位','','','','',0);",
        )
        .unwrap();
        let m = super::load_place_geo(&c);
        assert_eq!(m.len(), 2, "已解析的点位都要保留，未解析的跳过");
        let g = &m["40.037N 94.804E"];
        assert_eq!(g.county, "敦煌市");
        assert_eq!(g.address, "甘肃省酒泉市敦煌市鸣沙山");
        assert!(g.geocoded);
        // 旧数据没存详细地址，保留三级但仍需补一次详细地址
        let old = &m["旧版只有三级"];
        assert_eq!(old.city, "成都市");
        assert!(!old.geocoded);
    }

    #[test]
    fn parse_geocode_official_sample() {
        let body = r#"{"result":{"formatted_address":"海南省海口市秀英区滨海大道","addressComponent":{"province":"海南省","city":"海口市","county":"秀英区","nation":"中国"}},"status":"0","msg":"ok"}"#;
        let g = super::parse_geocode(body).unwrap();
        assert_eq!(
            (g.province.as_str(), g.city.as_str(), g.county.as_str()),
            ("海南省", "海口市", "秀英区")
        );
        assert_eq!(g.address, "海南省海口市秀英区滨海大道");
    }

    /// 直辖市 city 留空时，用 province 补齐，保证「市」级别有名字。
    #[test]
    fn parse_geocode_municipality_fills_city() {
        let body = r#"{"result":{"formatted_address":"北京市海淀区颐和园","addressComponent":{"province":"北京市","county":"海淀区"}},"status":"0"}"#;
        let g = super::parse_geocode(body).unwrap();
        assert_eq!(g.province, "北京市");
        assert_eq!(g.city, "北京市");
        assert_eq!(g.county, "海淀区");
    }

    /// 没有 formatted_address 时，用 addressComponent 的细粒度字段拼出详细地址。
    #[test]
    fn parse_geocode_falls_back_to_components() {
        let body = r#"{"result":{"addressComponent":{"province":"四川省","city":"成都市","county":"双流区","road":"天府大道","address":"100号","poi":"某园区"}},"status":"0"}"#;
        let g = super::parse_geocode(body).unwrap();
        assert_eq!(g.address, "四川省成都市双流区天府大道100号某园区");
    }

    #[test]
    fn parse_geocode_bad_status() {
        let body = r#"{"status":"1","msg":"key 无效"}"#;
        assert!(super::parse_geocode(body).is_err());
        assert!(super::parse_geocode("not json").is_err());
    }

    /// 429 的对外文案不能含 URL / tk。
    #[test]
    fn geocode_error_text_never_leaks_key() {
        let s = super::GeoErr::RateLimited.text();
        assert!(!s.contains("tk="));
        assert!(!s.contains("http"));
        assert!(s.contains("429"));
    }

    /// 修复迁移：被合并掉的同名 RAW 要被补回，JPG 与 RAW 各算一张（docs §32）。
    #[test]
    fn migrate_restores_missing_raw_member() {
        let dir = std::env::temp_dir().join("sdi-restore-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let jpg = dir.join("P9100001.JPG");
        let raw = dir.join("P9100001.RW2");
        std::fs::write(&jpg, b"j").unwrap();
        std::fs::write(&raw, b"r").unwrap();
        let jpg_s = jpg.to_string_lossy().to_string();

        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE albums (
                id INTEGER PRIMARY KEY AUTOINCREMENT, kind TEXT NOT NULL, name TEXT NOT NULL,
                cover_path TEXT, lat REAL, lon REAL, province TEXT, city TEXT,
                county TEXT, address TEXT, geocoded INTEGER, created_at TEXT NOT NULL
             );
             CREATE TABLE album_members (
                album_id INTEGER NOT NULL, path TEXT NOT NULL,
                confidence REAL, source TEXT NOT NULL, PRIMARY KEY (album_id, path)
             );
             CREATE TABLE gps_points (path TEXT PRIMARY KEY, lat REAL NOT NULL, lon REAL NOT NULL,
                shoot_time TEXT, place_id INTEGER) WITHOUT ROWID;
             CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;",
        )
        .unwrap();
        c.execute(
            "INSERT INTO albums (kind,name,cover_path,created_at) VALUES ('place','测试簇',?1,'2026-11-07 22:30:00')",
            params![jpg_s],
        )
        .unwrap();
        // 模拟 §31 的错误合并结果：只留下 JPG 一条，RAW 被删了
        c.execute(
            "INSERT INTO album_members (album_id,path,confidence,source) VALUES (1,?1,1.0,'gps')",
            params![jpg_s],
        )
        .unwrap();

        let smart = super::Smart {
            conn: Some(std::sync::Mutex::new(c)),
        };
        smart.migrate_restore_shot_pairs();

        let db = smart.conn.as_ref().unwrap().lock().unwrap();
        let mut paths: Vec<String> = db
            .prepare("SELECT path FROM album_members ORDER BY path")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .flatten()
            .collect();
        paths.sort();
        assert_eq!(paths.len(), 2, "JPG 与 RAW 应各有一条，恢复成两张独立文件");
        assert!(paths.iter().any(|p| p.ends_with(".JPG")));
        assert!(paths.iter().any(|p| p.ends_with(".RW2")));
        let ver: String = db
            .query_row("SELECT value FROM settings WHERE key='smart_schema'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, "3");

        // 幂等：再跑一次不重复插入
        drop(db);
        smart.migrate_restore_shot_pairs();
        let db = smart.conn.as_ref().unwrap().lock().unwrap();
        let n: i64 = db
            .query_row("SELECT COUNT(*) FROM album_members", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }
}
