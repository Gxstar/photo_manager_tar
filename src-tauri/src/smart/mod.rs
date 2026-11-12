//! 智能相册独立模块（docs/开发方案.md §17.6、§20 第一批）。
//!
//! 已实现的真实能力（不含任何假数据）：
//! - 独立的 `smart.db`（与 `gallery.db` 物理隔离，删掉即回到无 AI 状态）；
//! - 能力级模型清单 + 下载 / 校验 / 删除 + 模型与索引两级状态（docs §38）；
//! - 地点相册 / GPS 点位读写与坐标聚类；
//! - 人物相册手动建册（docs §35）。
//!
//! 已实现的 AI 能力（docs §38）：CN-CLIP 中文语义搜图 + 主体零样本分类、
//! YuNet + SFace 人脸检测与自动聚类（未命名簇 → 命名回填）。
//!
//! 边界：模型下载只由用户主动点击触发，绝不自动、不后台、不上报（§17.15）。
//! 人脸后台扫描需用户首次明确同意（`face_auto_scan` 设置），不同意就只走手动按钮。

pub(crate) mod clip;
mod face;
mod gps;

use crate::state::AppState;
use rusqlite::{params, Connection};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tauri::{ipc::Channel, AppHandle, Emitter, State};
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

/// 单个模型文件：`models/` 下的相对路径 + 期望字节数 + 下载地址 + 哈希。
///
/// `bytes == 0` = 字节数未知，只校验非空；`url` 为空 = 只能手动放入；
/// `sha256` 为空 = 尚未由本机核实（首下后写入 `model_files`，核实后再固化到这里）。
#[derive(Debug, Clone)]
pub struct ModelFile {
    pub path: &'static str,
    pub bytes: u64,
    pub url: &'static str,
    pub sha256: &'static str,
}

/// 一个**能力**（不是单个模型）：可独立下载、独立就绪、独立计费磁盘。
pub struct ModelSpec {
    pub key: &'static str,
    pub name: &'static str,
    pub desc: &'static str,
    pub license: &'static str,
    /// 索引版本：换模型必须改这里，旧向量不得与新向量混算（§17.6 ④）
    pub version: &'static str,
    pub size_mb: u32,
    pub files: &'static [ModelFile],
}

/// 能力级固定清单（docs §38.3）。体积与字节数为公开仓实测/模型卡口径，
/// 与 §17.8 的 280MB 估算不同：那是 int8 量化外推，现成 fp32 双塔约 723MB。
const MODELS: &[ModelSpec] = &[
    ModelSpec {
        key: "cnclip",
        name: "中文语义搜图",
        desc: "Chinese-CLIP ViT-B/16 int8（HF 官方生态转换），解锁语义搜图与「事物」分类",
        license: "Apache-2.0",
        version: "cnclip-vit-b16-q8-r2",
        size_mb: 191,
        files: &[
            ModelFile {
                path: "cnclip-vit-b16/model_quantized.onnx",
                bytes: 190_842_404,
                url: "https://hf-mirror.com/Xenova/chinese-clip-vit-base-patch16/resolve/main/onnx/model_quantized.onnx",
                sha256: "a2c2849329eafb24100acec5a9421a39f8cb445296444c5c98855b69e5e7a751",
            },
            ModelFile {
                path: "cnclip-vit-b16/tokenizer.json",
                bytes: 439_124,
                url: "https://hf-mirror.com/Xenova/chinese-clip-vit-base-patch16/resolve/main/tokenizer.json",
                sha256: "7dfbf1966ebf99d471c3796e9b457329d2b2182b817e144f1e904b957745c839",
            },
        ],
    },
    ModelSpec {
        key: "faces",
        name: "人物识别",
        desc: "OpenCV 官方 YuNet + SFace，解锁人脸检测、未命名人物簇与命名回填",
        license: "Apache-2.0",
        version: "yunet-2023mar-sface-2021dec-r1",
        size_mb: 39,
        files: &[
            ModelFile {
                path: "faces/yunet.onnx",
                bytes: 232_589,
                url: "https://hf-mirror.com/opencv/face_detection_yunet/resolve/main/face_detection_yunet_2023mar.onnx",
                sha256: "8f2383e4dd3cfbb4553ea8718107fc0423210dc964f9f4280604804ed2552fa4",
            },
            ModelFile {
                path: "faces/sface.onnx",
                bytes: 38_696_353,
                url: "https://hf-mirror.com/opencv/face_recognition_sface/resolve/main/face_recognition_sface_2021dec.onnx",
                sha256: "0ba9fbfa01b5270c96627c4ef784da859931e02f04419c829e83484087c34e79",
            },
        ],
    },
];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelFileStatus {
    pub path: String,
    pub bytes: u64,
    pub present: bool,
    pub actual_bytes: u64,
    /// 本机首次下载后记录的哈希（空 = 尚未记录）
    pub sha256: String,
}

/// 索引维度（与「模型文件是否就绪」正交，docs §38.5）。
/// state: `empty` 未建 / `partial` 部分可用 / `complete` 全库 / `stale` 模型已变更需重建
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexInfo {
    pub state: String,
    pub indexed: i64,
    pub total: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelStatus {
    pub key: String,
    pub name: String,
    pub desc: String,
    pub size_mb: u32,
    pub license: String,
    pub version: String,
    pub ready: bool,
    /// 已下载 / 已放入目录 / 未下载
    pub source: String,
    /// 缺失或大小不符的文件（相对路径）
    pub missing: Vec<String>,
    pub files: Vec<ModelFileStatus>,
    pub index: IndexInfo,
}

/// 下载进度：按**单文件**推送，前端用 `fileIndex / fileTotal` 表达整体位置。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadProgress {
    pub key: String,
    pub file: String,
    pub file_index: usize,
    pub file_total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub done: bool,
}

/// 下载完成的一个文件（供命令层写库）。
#[derive(Debug, Clone)]
pub struct DownloadedFile {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

/// 索引进度（docs §38.5）：按已处理张数推送，前端只显示，不弹窗。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexProgress {
    pub done: usize,
    pub total: usize,
    pub failed: usize,
    pub current: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexSummary {
    pub total: usize,
    pub indexed: usize,
    pub skipped: usize,
    pub failed: usize,
    pub cancelled: bool,
}

/// 语义搜索结果：score 为余弦相似度（向量已 L2 归一化，点积即余弦）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub path: String,
    pub score: f32,
}

/// 主体分类的一张卡：固定标签 + 命中数 + 代表图。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThingRow {
    pub label: String,
    pub count: i64,
    pub cover_path: Option<String>,
}

/// 人脸扫描进度（docs §38 人物批）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FaceProgress {
    pub done: usize,
    pub total: usize,
    pub faces: usize,
    pub current: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FaceSummary {
    pub total: usize,
    pub skipped: usize,
    pub faces: usize,
    pub clusters: usize,
    pub cancelled: bool,
}

/// 自动聚类簇：person_id 为空 = 待命名；ignored = 用户已忽略。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FaceClusterRow {
    pub id: i64,
    pub count: i64,
    pub cover_path: Option<String>,
    pub person_id: Option<i64>,
    pub ignored: bool,
}

/// 人脸聚类的余弦阈值：SFace 官方同人阈值 0.363（对齐后）；这里未做关键点对齐，
    /// 实测同人 0.89、阈值放宽到 0.50 以减少误合并。
const FACE_CLUSTER_THRESHOLD: f32 = 0.36;

/// 每个簇保留的样例向量数：分配时与「最近像的那个成员」比，减少同人被拆簇。
const MAX_EXEMPLARS: usize = 24;

/// 人脸扫描版本：换检测分辨率 / 换模型后，旧向量不能与新向量混算，
/// 必须清空重扫（§17.6 ④ 的同一条约束）。
/// r1 = 256px 缩略图 + 仅质心；r2 = 640px 原图 + 样例比对 + 人脸封面。
pub const FACE_SCAN_VER: &str = "sface-640-r2";
/// 已写入 `smart.db` 的扫描版本（settings 表）。
pub const FACE_SCAN_VER_KEY: &str = "face_scan_ver";

/// 人脸封面目录：对齐后的 112×112 JPEG，命名 `c<cluster_id>.jpg`。
/// 与 `smart.db` 同级的独立目录，删库不影响、清数据时一并删掉。
pub fn face_covers_dir() -> PathBuf {
    base_dir().join("face_covers")
}

/// 一个簇的运行时状态（只在扫描期间存在）。
struct ClusterState {
    id: i64,
    proto: Vec<f32>,
    count: i64,
    person_id: Option<i64>,
    exemplars: Vec<Vec<f32>>,
}

/// 后台自动扫描的用户意愿（docs §38 人物批）。
/// `ask` = 还没问过；`on` = 允许开机后台扫；`off` = 只手动扫。
pub const FACE_AUTO_KEY: &str = "face_auto_scan";
/// 自动扫描启动后的等待时间：先让首屏、图库、缩略图缓冲都跑完再碰磁盘。
const FACE_AUTO_DELAY_SECS: u64 = 20;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FaceAutoScan {
    /// ask | on | off
    pub consent: String,
    pub model_ready: bool,
    /// 当前有扫描线程在跑（含等待启动的自动扫描）
    pub running: bool,
    /// 已扫描过的照片数（`face_scanned` 行数）
    pub scanned: i64,
    /// 已有人脸簇总数（含已命名 / 已忽略）
    pub clusters: i64,
    /// 旧结果的扫描版本与当前算法不一致：必须清除后重扫
    pub stale: bool,
}

fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

fn blob_to_vec(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// 进行中的下载：key → 取消标记。
/// 用进程级静态表而不是 `AppState` 字段，是为了让 `spawn_blocking` 内部也能读到取消信号。
fn downloads() -> &'static Mutex<HashMap<String, Arc<AtomicBool>>> {
    static DOWNLOADS: std::sync::OnceLock<Mutex<HashMap<String, Arc<AtomicBool>>>> =
        std::sync::OnceLock::new();
    DOWNLOADS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn file_ready(path: &Path, f: &ModelFile) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.len() > 0 && (f.bytes == 0 || m.len() == f.bytes))
        .unwrap_or(false)
}

fn find_spec(key: &str) -> Result<&'static ModelSpec, String> {
    MODELS
        .iter()
        .find(|s| s.key == key)
        .ok_or_else(|| format!("未知的模型能力：{key}"))
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

/// 人物相册（docs §35）。复用 `albums(kind='person')` + `album_members(source='user')`，
/// 零新表、零 schema 变更；`coverPath` 为展示封面（用户指定优先，否则回退成员首图）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PersonRow {
    pub id: i64,
    pub name: String,
    pub cover_path: Option<String>,
    pub count: i64,
    pub created_at: String,
}

/// 关联 / 取消关联的结果。重复操作不算失败，但要如实回报数字，
/// 前端据此提示「新增 N 张 · M 张已在该人物中，已跳过」。
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkReport {
    pub added: usize,
    pub duplicates: usize,
    pub removed: usize,
    pub not_linked: usize,
    /// 路径为空 / 文件已不在库中，未写入索引
    pub skipped: usize,
}

/// 人物查询统一列顺序：id / name / 展示封面 / 成员数 / created_at。
/// 封面为空时回退成员首图（只影响展示，不写库），避免新人物刚关联照片时封面空白。
fn person_select(where_sql: &str) -> String {
    format!(
        "SELECT a.id, a.name,
                COALESCE(a.cover_path,
                    (SELECT m.path FROM album_members m WHERE m.album_id = a.id ORDER BY m.path LIMIT 1)),
                (SELECT COUNT(*) FROM album_members m WHERE m.album_id = a.id),
                a.created_at
         FROM albums a {where_sql}"
    )
}

fn map_person_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<PersonRow> {
    Ok(PersonRow {
        id: r.get(0)?,
        name: r.get(1)?,
        cover_path: r.get(2)?,
        count: r.get(3)?,
        created_at: r.get(4)?,
    })
}

fn person_name(name: &str) -> Result<&str, String> {
    let v = name.trim();
    if v.is_empty() {
        return Err("人物名字不能为空".into());
    }
    if v.chars().count() > 40 {
        return Err("人物名字最多 40 个字".into());
    }
    Ok(v)
}

/// 同名（大小写不敏感）检查；rename 时排除自己。
fn person_name_exists(
    db: &Connection,
    name: &str,
    exclude_id: Option<i64>,
) -> Result<bool, String> {
    let found = match exclude_id {
        Some(id) => db.query_row(
            "SELECT EXISTS(SELECT 1 FROM albums WHERE kind = 'person'
               AND name = ?1 COLLATE NOCASE AND id <> ?2)",
            params![name, id],
            |r| r.get::<_, i64>(0),
        ),
        None => db.query_row(
            "SELECT EXISTS(SELECT 1 FROM albums WHERE kind = 'person'
               AND name = ?1 COLLATE NOCASE)",
            params![name],
            |r| r.get::<_, i64>(0),
        ),
    };
    let n = found.map_err(|e| format!("读取人物失败: {e}"))?;
    Ok(n != 0)
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
                    ) WITHOUT ROWID;
                    -- 索引主表：一行 = 一张已算过向量的照片（docs §38.6）
                    CREATE TABLE IF NOT EXISTS media (
                        path       TEXT PRIMARY KEY,
                        mtime      INTEGER NOT NULL,
                        size       INTEGER NOT NULL,
                        model_ver  TEXT    NOT NULL,
                        clip_vec   BLOB,
                        indexed_at TEXT,
                        failed     INTEGER NOT NULL DEFAULT 0
                    ) WITHOUT ROWID;
                    -- 索引任务：单行状态，进度可续跑
                    CREATE TABLE IF NOT EXISTS index_jobs (
                        model_key  TEXT PRIMARY KEY,
                        state      TEXT NOT NULL DEFAULT 'idle',
                        total      INTEGER NOT NULL DEFAULT 0,
                        done       INTEGER NOT NULL DEFAULT 0,
                        updated_at TEXT NOT NULL
                    ) WITHOUT ROWID;
                    -- 每个模型文件的实测哈希：首下后写入，用于下次硬校验与固化清单
                    CREATE TABLE IF NOT EXISTS model_files (
                        key        TEXT NOT NULL,
                        path       TEXT NOT NULL,
                        sha256     TEXT,
                        bytes      INTEGER,
                        checked_at TEXT,
                        PRIMARY KEY (key, path)
                    ) WITHOUT ROWID;
                    -- 主体分类：每张图 top-N 零样本标签（由已有 clip_vec 打分，不重复推理）
                    CREATE TABLE IF NOT EXISTS clip_tags (
                        path  TEXT NOT NULL,
                        label TEXT NOT NULL,
                        score REAL NOT NULL,
                        PRIMARY KEY (path, label)
                    ) WITHOUT ROWID;
                    CREATE INDEX IF NOT EXISTS idx_clip_tags_label ON clip_tags(label);
                    -- 人脸：一张图可多张脸；cluster_id 指向自动聚类簇
                    CREATE TABLE IF NOT EXISTS faces (
                        id         INTEGER PRIMARY KEY AUTOINCREMENT,
                        path       TEXT NOT NULL,
                        x1         REAL NOT NULL,
                        y1         REAL NOT NULL,
                        x2         REAL NOT NULL,
                        y2         REAL NOT NULL,
                        score      REAL NOT NULL,
                        vec        BLOB NOT NULL,
                        cluster_id INTEGER,
                        created_at TEXT NOT NULL
                    );
                    CREATE INDEX IF NOT EXISTS idx_faces_path ON faces(path);
                    CREATE INDEX IF NOT EXISTS idx_faces_cluster ON faces(cluster_id);
                    -- 自动聚类簇：person_id 非空表示已命名并绑定到 albums(kind='person')
                    CREATE TABLE IF NOT EXISTS face_clusters (
                        id         INTEGER PRIMARY KEY AUTOINCREMENT,
                        person_id  INTEGER,
                        cover_path TEXT,
                        proto      BLOB NOT NULL,
                        count      INTEGER NOT NULL DEFAULT 0,
                        ignored    INTEGER NOT NULL DEFAULT 0,
                        created_at TEXT NOT NULL
                    );
                    -- 已扫描过的照片指纹（path + mtime），避免重复检测
                    CREATE TABLE IF NOT EXISTS face_scanned (
                        path  TEXT PRIMARY KEY,
                        mtime INTEGER NOT NULL
                    ) WITHOUT ROWID;"
                )?;
                // 已有 model_sources 表补 version 列；重复执行报 duplicate column，忽略即可。
                let _ = c.execute("ALTER TABLE model_sources ADD COLUMN version TEXT", []);
                // 第一批已建过 albums 表时补列；重复执行会报 duplicate column，忽略即可。
                let _ = c.execute("ALTER TABLE albums ADD COLUMN lat REAL", []);
                let _ = c.execute("ALTER TABLE albums ADD COLUMN lon REAL", []);
                let _ = c.execute("ALTER TABLE albums ADD COLUMN province TEXT", []);
                let _ = c.execute("ALTER TABLE albums ADD COLUMN city TEXT", []);
                let _ = c.execute("ALTER TABLE albums ADD COLUMN county TEXT", []);
                let _ = c.execute("ALTER TABLE albums ADD COLUMN address TEXT", []);
                let _ = c.execute("ALTER TABLE albums ADD COLUMN geocoded INTEGER", []);
                // 零样本置信度（softmax top1 概率），用于「宁可不归类也别错归」的门槛
                let _ = c.execute("ALTER TABLE clip_tags ADD COLUMN conf REAL", []);
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

    // ── 模型清单 / 下载 / 索引状态（docs §38）────────────────────────────

    /// 本机记录的模型文件哈希：首下时写入，下次下载可据此硬校验。
    fn model_file_hashes(&self, key: &str) -> HashMap<String, String> {
        let Some(db) = self.conn.as_ref().and_then(|c| c.lock().ok()) else {
            return HashMap::new();
        };
        let Ok(mut stmt) =
            db.prepare("SELECT path, COALESCE(sha256, '') FROM model_files WHERE key = ?1")
        else {
            return HashMap::new();
        };
        match stmt.query_map(params![key], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        }) {
            Ok(it) => it.flatten().collect(),
            Err(_) => HashMap::new(),
        }
    }

    /// 模型来源：`download`（应用内下载）/ 无记录（手动放入）。只影响 UI 文案。
    fn model_source_kind(&self, key: &str) -> Option<String> {
        let db = self.conn.as_ref()?.lock().ok()?;
        db.query_row(
            "SELECT source FROM model_sources WHERE key = ?1",
            params![key],
            |r| r.get::<_, String>(0),
        )
        .ok()
    }

    /// 索引维度与「模型是否就绪」正交：没有 media 行就是 `empty`，不冒充可用。
    pub fn index_info(&self, spec: &ModelSpec) -> IndexInfo {
        let Some(db) = self.conn.as_ref().and_then(|c| c.lock().ok()) else {
            return IndexInfo {
                state: "empty".into(),
                indexed: 0,
                total: 0,
            };
        };
        let indexed: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM media WHERE model_ver = ?1",
                params![spec.version],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let stale: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM media WHERE model_ver <> ?1",
                params![spec.version],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let total: i64 = db
            .query_row(
                "SELECT COALESCE(total, 0) FROM index_jobs WHERE model_key = ?1",
                params![spec.key],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let state = if stale > 0 {
            "stale"
        } else if indexed == 0 {
            "empty"
        } else if total > 0 && indexed < total {
            "partial"
        } else {
            "complete"
        };
        IndexInfo {
            state: state.into(),
            indexed,
            total,
        }
    }

    /// 下载成功后落库：文件级哈希 + 能力级来源 / 版本。
    /// 只写 smart.db，不动照片文件；失败时如实返回，不静默成功。
    pub fn record_model_files(&self, key: &str, files: &[DownloadedFile]) -> Result<(), String> {
        let Some(conn) = &self.conn else {
            return Err("smart.db 不可用".into());
        };
        let db = conn.lock().map_err(|_| "smart.db 被占用".to_string())?;
        let now = chrono::Local::now()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        for f in files {
            db.execute(
                "INSERT INTO model_files (key, path, sha256, bytes, checked_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(key, path) DO UPDATE SET
                     sha256 = COALESCE(NULLIF(excluded.sha256, ''), model_files.sha256),
                     bytes = excluded.bytes, checked_at = excluded.checked_at",
                params![key, f.path, f.sha256, f.bytes as i64, now],
            )
            .map_err(|e| format!("记录模型文件失败: {e}"))?;
        }
        let version = find_spec(key)?.version;
        db.execute(
            "INSERT INTO model_sources (key, source, updated_at, version)
             VALUES (?1, 'download', ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET source = 'download',
                 updated_at = excluded.updated_at, version = excluded.version",
            params![key, now, version],
        )
        .map_err(|e| format!("记录模型来源失败: {e}"))?;
        Ok(())
    }

    /// 删除模型文件释放磁盘；**索引保留**（§17.15：重跑一次索引很贵）。
    /// 状态随之回到未就绪，`media` 行不动。
    pub fn delete_model(&self, key: &str) -> Result<u64, String> {
        let spec = find_spec(key)?;
        let root = models_dir();
        let mut freed = 0u64;
        for f in spec.files {
            let target = root.join(f.path);
            if let Ok(m) = std::fs::metadata(&target) {
                if m.is_file() {
                    freed += m.len();
                    std::fs::remove_file(&target)
                        .map_err(|e| format!("删除 {} 失败: {e}", f.path))?;
                }
            }
            // 未完成的 .part 一并清掉，避免下次误判
            let part = target.with_file_name(format!(
                "{}.part",
                target
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("model")
            ));
            let _ = std::fs::remove_file(&part);
        }
        if let Some(conn) = &self.conn {
            if let Ok(db) = conn.lock() {
                let _ = db.execute("DELETE FROM model_files WHERE key = ?1", params![key]);
                let _ = db.execute("DELETE FROM model_sources WHERE key = ?1", params![key]);
            }
        }
        Ok(freed)
    }

    /// 语义搜索：文本塔编码查询，与全库向量点积（已归一化 → 余弦）。

    /// 全库增量索引：指纹 = path + mtime + size + model_ver，未变即跳过。
    /// 只用 256px 缩略图（复用 thumbs 缓存），避免为索引重读原图；
    /// 每张独立短事务，搜索不会被长时间锁住；可随时取消，已完成的照片保留。
    pub fn index_clip<F>(
        &self,
        root: &str,
        original_dir: &str,
        cancel: &AtomicBool,
        mut progress: F,
    ) -> Result<IndexSummary, String>
    where
        F: FnMut(IndexProgress),
    {
        let spec = find_spec("cnclip")?;
        if !clip::model_present() {
            return Err("语义搜图模型未下载".into());
        }
        let base = Path::new(root).join(original_dir);
        if !base.is_dir() {
            return Err(format!("媒体库目录不可用：{}", base.display()));
        }
        let Some(conn) = &self.conn else {
            return Err("smart.db 不可用".into());
        };

        let mut files: Vec<(String, i64, u64)> = Vec::new();
        for entry in WalkDir::new(&base).into_iter().filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            let p = entry.path();
            if !is_shot_file(p) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            files.push((p.to_string_lossy().to_string(), mtime, meta.len()));
        }
        let total = files.len();
        let now = chrono::Local::now()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();

        {
            let db = conn.lock().map_err(|_| "smart.db 被占用".to_string())?;
            db.execute(
                "INSERT INTO index_jobs (model_key, state, total, done, updated_at)
                 VALUES (?1, 'running', ?2, 0, ?3)
                 ON CONFLICT(model_key) DO UPDATE SET state = 'running',
                     total = excluded.total, done = 0, updated_at = excluded.updated_at",
                params![spec.key, total as i64, now],
            )
            .map_err(|e| format!("写入索引任务失败: {e}"))?;
        }

        let (mut indexed, mut skipped, mut failed, mut cancelled) = (0usize, 0, 0, false);
        for (i, (path, mtime, size)) in files.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }
            let fresh = {
                let db = conn.lock().map_err(|_| "smart.db 被占用".to_string())?;
                db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM media
                       WHERE path = ?1 AND mtime = ?2 AND size = ?3 AND model_ver = ?4)",
                    params![path, mtime, *size as i64, spec.version],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0)
                    != 0
            };
            if fresh {
                skipped += 1;
            } else {
                // 复用缩略图缓存；未命中才生成（与图库共用 256px JPEG）
                let thumb = crate::thumbs::lookup(Path::new(path), *mtime, *size)
                    .or_else(|| crate::thumbs::create(Path::new(path), *mtime, *size));
                let Some(bytes) = thumb else {
                    failed += 1;
                    let _ = conn.lock().map(|db| {
                        db.execute(
                            "INSERT INTO media (path, mtime, size, model_ver, clip_vec, indexed_at, failed)
                             VALUES (?1, ?2, ?3, ?4, NULL, ?5, 1)
                             ON CONFLICT(path) DO UPDATE SET failed = 1",
                            params![path, mtime, *size as i64, spec.version, now],
                        )
                    });
                    continue;
                };
                let img = match clip::decode_thumb(&bytes) {
                    Ok(v) => v,
                    Err(_) => {
                        failed += 1;
                        continue;
                    }
                };
                let vec = clip::with_clip(|c| c.encode_image(&img))?;
                // 同一份图像向量直接打零样本标签，不额外推理
                let assigned = clip::assign_thing(&vec);
                let blob = vec_to_blob(&vec);
                let db = conn.lock().map_err(|_| "smart.db 被占用".to_string())?;
                db.execute(
                    "INSERT INTO media (path, mtime, size, model_ver, clip_vec, indexed_at, failed)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)
                     ON CONFLICT(path) DO UPDATE SET mtime = excluded.mtime,
                         size = excluded.size, model_ver = excluded.model_ver,
                         clip_vec = excluded.clip_vec, indexed_at = excluded.indexed_at,
                         failed = 0",
                    params![path, mtime, *size as i64, spec.version, blob, now],
                )
                .map_err(|e| format!("写入向量失败: {e}"))?;
                let _ = db.execute("DELETE FROM clip_tags WHERE path = ?1", params![path]);
                // 每张图只归一个主体类：cos 与置信度双门槛，宁可少归不要错归
                if let Some((label, score, conf)) = assigned {
                    let _ = db.execute(
                        "INSERT OR REPLACE INTO clip_tags (path, label, score, conf)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![path, label, score, conf],
                    );
                }
                drop(db);
                indexed += 1;
            }

            if i % 10 == 0 || i + 1 == total {
                let name = Path::new(path)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                progress(IndexProgress {
                    done: i + 1,
                    total,
                    failed,
                    current: name,
                });
                if let Ok(db) = conn.lock() {
                    let _ = db.execute(
                        "UPDATE index_jobs SET done = ?1, updated_at = ?2 WHERE model_key = ?3",
                        params![(i + 1) as i64, now, spec.key],
                    );
                }
            }
        }

        {
            let db = conn.lock().map_err(|_| "smart.db 被占用".to_string())?;
            let state = if cancelled { "idle" } else { "done" };
            let _ = db.execute(
                "UPDATE index_jobs SET state = ?1, done = ?2, updated_at = ?3 WHERE model_key = ?4",
                params![state, (indexed + skipped) as i64, now, spec.key],
            );
        }
        Ok(IndexSummary {
            total,
            indexed,
            skipped,
            failed,
            cancelled,
        })
    }

    /// 用已有 `clip_vec` 回填零样本标签（不重新推理），约 1–2 秒。
    /// 索引完成后自动触发一次；已建过则不重复。
    pub fn backfill_tags(&self) -> Result<usize, String> {
        let spec = find_spec("cnclip")?;
        let Some(conn) = &self.conn else {
            return Err("smart.db 不可用".into());
        };
        let rows: Vec<(String, Vec<u8>)> = {
            let db = conn.lock().map_err(|_| "smart.db 被占用".to_string())?;
            let mut stmt = db
                .prepare(
                    "SELECT path, clip_vec FROM media
                     WHERE model_ver = ?1 AND clip_vec IS NOT NULL",
                )
                .map_err(|e| format!("读取向量失败: {e}"))?;
            stmt.query_map(params![spec.version], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
            })
            .map_err(|e| format!("读取向量失败: {e}"))?
            .flatten()
            .collect()
        };
        // 标签向量在锁外准备（首次会加载模型，约 1 秒）
        let labels = clip::thing_vectors()?;
        let mut db = conn.lock().map_err(|_| "smart.db 被占用".to_string())?;
        let tx = db.transaction().map_err(|e| format!("写入标签失败: {e}"))?;
        tx.execute("DELETE FROM clip_tags", [])
            .map_err(|e| format!("写入标签失败: {e}"))?;
        let mut n = 0usize;
        {
            let mut ins = tx
                .prepare(
                    "INSERT OR REPLACE INTO clip_tags (path, label, score, conf)
                     VALUES (?1, ?2, ?3, ?4)",
                )
                .map_err(|e| format!("写入标签失败: {e}"))?;
            for (path, blob) in &rows {
                let v = blob_to_vec(blob);
                if v.len() != 512 {
                    continue;
                }
                let mut scored: Vec<(&str, f32)> = labels
                    .iter()
                    .map(|(label, tv)| {
                        (
                            *label,
                            v.iter().zip(tv.iter()).map(|(a, b)| a * b).sum::<f32>(),
                        )
                    })
                    .collect();
                scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                // 与索引流程同一套判定：cos + softmax 置信度双门槛，每张图只归一类
                let conf = clip::thing_confidence(&scored);
                if let Some((label, score)) = scored.first() {
                    if *score >= clip::THING_MIN_COS && conf >= clip::THING_MIN_CONF {
                        ins.execute(params![path, *label, *score, conf])
                            .map_err(|e| format!("写入标签失败: {e}"))?;
                        n += 1;
                    }
                }
            }
        }
        tx.commit().map_err(|e| format!("写入标签失败: {e}"))?;
        Ok(n)
    }

    /// 主体分类列表：标签 + 数量 + 代表图（排除废片）。
    /// 第一次进入时若已有向量但没标签，自动回填一次（用户无感，约 1–2 秒）。
    pub fn things(&self, rejects: &HashSet<String>) -> Vec<ThingRow> {
        let need_backfill = {
            let Some(db) = self.conn.as_ref().and_then(|c| c.lock().ok()) else {
                return Vec::new();
            };
            let tags: i64 = db
                .query_row("SELECT COUNT(*) FROM clip_tags", [], |r| r.get(0))
                .unwrap_or(0);
            let media: i64 = db
                .query_row(
                    "SELECT COUNT(*) FROM media WHERE clip_vec IS NOT NULL",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            tags == 0 && media > 0
        };
        if need_backfill {
            let _ = self.backfill_tags();
        }
        let Some(db) = self.conn.as_ref().and_then(|c| c.lock().ok()) else {
            return Vec::new();
        };
        let counts: Vec<(String, i64)> = {
            let Ok(mut stmt) = db.prepare(
                "SELECT label, COUNT(*) FROM clip_tags GROUP BY label ORDER BY COUNT(*) DESC",
            ) else {
                return Vec::new();
            };
            match stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))) {
                Ok(it) => it.flatten().collect(),
                Err(_) => Vec::new(),
            }
        };
        let mut out = Vec::new();
        for (label, count) in counts {
            let mut cover = None;
            if let Ok(mut stmt) = db
                .prepare("SELECT path FROM clip_tags WHERE label = ?1 ORDER BY score DESC LIMIT 8")
            {
                if let Ok(it) = stmt.query_map(params![label], |r| r.get::<_, String>(0)) {
                    cover = it.flatten().find(|p| !rejects.contains(p));
                }
            }
            out.push(ThingRow {
                label,
                count,
                cover_path: cover,
            });
        }
        out
    }

    pub fn thing_photos(&self, label: &str, rejects: &HashSet<String>) -> Vec<String> {
        let Some(db) = self.conn.as_ref().and_then(|c| c.lock().ok()) else {
            return Vec::new();
        };
        let Ok(mut stmt) =
            db.prepare("SELECT path FROM clip_tags WHERE label = ?1 ORDER BY score DESC")
        else {
            return Vec::new();
        };
        match stmt.query_map(params![label], |r| r.get::<_, String>(0)) {
            Ok(it) => it
                .flatten()
                .filter(|p| !rejects.contains(p))
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// 语义搜索：文本塔编码查询，与全库向量点积（已归一化 → 余弦）。
    pub fn search_clip(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
        let q = clip::with_clip(|c| c.encode_text(query))?;
        if q.len() != 512 {
            return Err("查询向量维度异常".into());
        }
        let spec = find_spec("cnclip")?;
        let Some(conn) = &self.conn else {
            return Err("smart.db 不可用".into());
        };
        let db = conn.lock().map_err(|_| "smart.db 被占用".to_string())?;
        let mut stmt = db
            .prepare(
                "SELECT path, clip_vec FROM media
                 WHERE model_ver = ?1 AND clip_vec IS NOT NULL AND failed = 0",
            )
            .map_err(|e| format!("读取索引失败: {e}"))?;
        let rows = stmt
            .query_map(params![spec.version], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
            })
            .map_err(|e| format!("读取索引失败: {e}"))?;
        let mut hits: Vec<SearchHit> = Vec::new();
        for (path, blob) in rows.flatten() {
            let v = blob_to_vec(&blob);
            if v.len() != q.len() {
                continue;
            }
            let score: f32 = v.iter().zip(q.iter()).map(|(a, b)| a * b).sum();
            hits.push(SearchHit { path, score });
        }
        hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(limit.clamp(1, 200));
        Ok(hits)
    }

    // ── 人脸扫描与自动聚类（docs §38 人物批）──────────────────────────────

    /// 全库人脸扫描：用 256px 缩略图（YuNet 输入固定 640，小图会被放大，仍可检出），
    /// 每张图检测 + 编码 + 贪心聚类；`face_scanned(path, mtime)` 保证增量。
    /// 已命名簇（person_id 非空）会把路径同步写进 `album_members`，人物册即时更新。
    pub fn scan_faces<F>(
        &self,
        root: &str,
        original_dir: &str,
        max_files: usize,
        cancel: &AtomicBool,
        mut progress: F,
    ) -> Result<FaceSummary, String>
    where
        F: FnMut(FaceProgress),
    {
        if !face::models_present() {
            return Err("人脸模型未下载".into());
        }
        let base = Path::new(root).join(original_dir);
        if !base.is_dir() {
            return Err(format!("媒体库目录不可用：{}", base.display()));
        }
        let Some(conn) = &self.conn else {
            return Err("smart.db 不可用".into());
        };

        // 版本闸门：旧版（256px 缩略图）算出的向量与新版不在同一尺度上，
        // 混在一起会让聚类彻底失真。宁可拒绝，也不产出「看着能跑但结果更差」的数据。
        let (_, old_clusters) = self.face_stats();
        if old_clusters > 0 && self.get_setting(FACE_SCAN_VER_KEY).as_deref() != Some(FACE_SCAN_VER)
        {
            return Err(
                "人脸识别算法已更新（改用 640px 原图），旧结果与新结果不能混用：\
                 请先在「人物」段点「清除人脸数据」，再重新扫描"
                    .into(),
            );
        }

        let mut files: Vec<(String, i64)> = Vec::new();
        for entry in WalkDir::new(&base).into_iter().filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            let p = entry.path();
            if !is_shot_file(p) {
                continue;
            }
            // RAW 解码 / 内嵌预览太慢，本轮只扫可直接解码的格式
            let ext = p
                .extension()
                .and_then(|x| x.to_str())
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();
            if !matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "webp") {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            files.push((p.to_string_lossy().to_string(), mtime));
        }
        files.sort();
        // max_files > 0 时均匀取样（调试 / 验证用）；正式扫描传 0，全量
        if max_files > 0 && files.len() > max_files {
            let step = (files.len() / max_files).max(1);
            files = files.into_iter().step_by(step).take(max_files).collect();
        }
        let total = files.len();

        let scanned: HashSet<String> = {
            let db = conn.lock().map_err(|_| "smart.db 被占用".to_string())?;
            let Ok(mut stmt) = db.prepare("SELECT path FROM face_scanned") else {
                return Err("读取扫描记录失败".into());
            };
            match stmt.query_map([], |r| r.get::<_, String>(0)) {
                Ok(it) => it.flatten().collect(),
                Err(_) => HashSet::new(),
            }
        };
        // 簇状态：质心用于判断「是不是同一类」，样例向量用于判断「像不像某个成员」。
        // 只跟质心比会让同一个人在不同光线 / 角度下被拆成多个簇——
        // 这是「很多是同一个人却没聚到一起」的直接原因之一。
        let mut clusters: Vec<ClusterState> = {
            let db = conn.lock().map_err(|_| "smart.db 被占用".to_string())?;
            let Ok(mut stmt) =
                db.prepare("SELECT id, proto, count, person_id FROM face_clusters WHERE ignored = 0")
            else {
                return Err("读取人脸簇失败".into());
            };
            match stmt.query_map([], |r| {
                Ok(ClusterState {
                    id: r.get(0)?,
                    proto: blob_to_vec(&r.get::<_, Vec<u8>>(1)?),
                    count: r.get(2)?,
                    person_id: r.get(3)?,
                    exemplars: Vec::new(),
                })
            }) {
                Ok(it) => it.flatten().collect(),
                Err(_) => Vec::new(),
            }
        };
        // 每簇取最近的若干张人脸当样例（新的优先）
        {
            let db = conn.lock().map_err(|_| "smart.db 被占用".to_string())?;
            if let Ok(mut stmt) = db.prepare("SELECT cluster_id, vec FROM faces ORDER BY id DESC") {
                if let Ok(rows) = stmt.query_map([], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
                }) {
                    for (cid, blob) in rows.flatten() {
                        if let Some(c) = clusters.iter_mut().find(|c| c.id == cid) {
                            if c.exemplars.len() < MAX_EXEMPLARS {
                                c.exemplars.push(blob_to_vec(&blob));
                            }
                        }
                    }
                }
            }
        }

        let now = chrono::Local::now()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        // 没有新增照片就直接收工：自动扫描每次启动都会走一遍这里，
        // 不该为了「什么都没变」推上千条进度事件，也不该让进度条闪一下。
        let pending = files
            .iter()
            .filter(|(p, _)| !scanned.contains(p))
            .count();
        if pending == 0 {
            return Ok(FaceSummary {
                total,
                skipped: total,
                faces: 0,
                clusters: clusters.len(),
                cancelled: false,
            });
        }
        let (mut done, mut skipped, mut face_count, mut cancelled) = (0usize, 0, 0, false);
        // 进度按「每 20 张 / 满 1 秒 / 收尾」推：一张照片有脸时要解码原图（debug 5.5s），
        // 只按张数推会让界面长时间停在同一个数字上，看起来像卡死。
        let mut last_emit = std::time::Instant::now();

        for (path, mtime) in &files {
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }
            if scanned.contains(path) {
                skipped += 1;
                done += 1;
                continue;
            }
            let Ok(meta) = std::fs::metadata(path) else {
                skipped += 1;
                done += 1;
                continue;
            };
            // 两段式：先用 256px 缩略图判断「有没有脸」（缓存命中约 5ms），
            // 没有就绝不解码原图——debug 下解码一张 24MP JPEG 要 5.5 秒，
            // 这正是「扫描看着像卡住」的真正原因。有脸才解原图拿真实像素。
            let Some(bytes) = crate::thumbs::lookup(Path::new(path), *mtime, meta.len())
                .or_else(|| crate::thumbs::create(Path::new(path), *mtime, meta.len()))
            else {
                skipped += 1;
                done += 1;
                continue;
            };
            let Ok(thumb) = clip::decode_thumb(&bytes) else {
                skipped += 1;
                done += 1;
                continue;
            };
            let any_face = face::with_faces(|e| e.detect(&thumb).map(|f| !f.is_empty()));
            match any_face {
                Ok(true) => {}
                Ok(false) => {
                    // 缩略图都检不到脸：解原图大概率也检不到（YuNet 输入恒为 640），
                    // 直接记录已扫，省掉一次昂贵的解码
                    let db = conn.lock().map_err(|_| "smart.db 被占用".to_string())?;
                    let _ = db.execute(
                        "INSERT OR REPLACE INTO face_scanned (path, mtime) VALUES (?1, ?2)",
                        params![path, mtime],
                    );
                    drop(db);
                    done += 1;
                    if done % 20 == 0 || done == total || last_emit.elapsed().as_secs() >= 1 {
                        last_emit = std::time::Instant::now();
                        progress(FaceProgress {
                            done,
                            total,
                            faces: face_count,
                            current: Path::new(path)
                                .file_name()
                                .map(|s| s.to_string_lossy().to_string())
                                .unwrap_or_default(),
                        });
                    }
                    continue;
                }
                Err(e) => return Err(e),
            }
            // 有脸：解码原图到 640（关键点更准、编码用的是真实像素）；失败回退缩略图
            let img = face::load_for_scan(Path::new(path), face::SCAN_MAX_SIDE)
                .unwrap_or(thumb);
            let detected = face::with_faces(|e| {
                let fs = e.detect(&img)?;
                let mut vs = Vec::with_capacity(fs.len());
                for f in &fs {
                    vs.push(e.embed(&img, f)?);
                }
                Ok((fs, vs))
            });
            let (fs, vs) = match detected {
                Ok(v) => v,
                Err(e) => return Err(e),
            };
            if !fs.is_empty() {
                let mut db = conn.lock().map_err(|_| "smart.db 被占用".to_string())?;
                let tx = db.transaction().map_err(|e| format!("写入人脸失败: {e}"))?;
                for (f, (v, crop)) in fs.iter().zip(vs.iter()) {
                    let mut best: Option<(usize, f32)> = None;
                    for (i, c) in clusters.iter().enumerate() {
                        let cen: f32 = c.proto.iter().zip(v.iter()).map(|(a, b)| a * b).sum();
                        // 与簇内样例取最大：同一个人某个角度对质心可能只有 0.3，
                        // 但对「和自己最像的那张」往往 ≥0.5
                        let ex = c
                            .exemplars
                            .iter()
                            .map(|e| e.iter().zip(v.iter()).map(|(a, b)| a * b).sum::<f32>())
                            .fold(f32::MIN, f32::max);
                        let score = cen.max(ex);
                        if best.map(|(_, s)| score > s).unwrap_or(true) {
                            best = Some((i, score));
                        }
                    }
                    let cluster_id = match best {
                        Some((i, c)) if c >= FACE_CLUSTER_THRESHOLD => {
                            let cluster = &mut clusters[i];
                            let new_count = cluster.count + 1;
                            let old = cluster.count as f32;
                            for (p, x) in cluster.proto.iter_mut().zip(v.iter()) {
                                *p = (*p * old + *x) / new_count as f32;
                            }
                            let norm: f32 =
                                cluster.proto.iter().map(|x| x * x).sum::<f32>().sqrt();
                            if norm > 1e-6 {
                                for p in cluster.proto.iter_mut() {
                                    *p /= norm;
                                }
                            }
                            cluster.count = new_count;
                            if cluster.exemplars.len() < MAX_EXEMPLARS {
                                cluster.exemplars.push(v.clone());
                            }
                            let pid = cluster.person_id;
                            let blob = vec_to_blob(&cluster.proto);
                            tx.execute(
                                "UPDATE face_clusters SET proto = ?1, count = ?2 WHERE id = ?3",
                                params![blob, new_count, cluster.id],
                            )
                            .map_err(|e| format!("更新人脸簇失败: {e}"))?;
                            if let Some(pid) = pid {
                                tx.execute(
                                    "INSERT OR IGNORE INTO album_members
                                         (album_id, path, confidence, source)
                                     VALUES (?1, ?2, ?3, 'face')",
                                    params![pid, path, c],
                                )
                                .map_err(|e| format!("写入人物册失败: {e}"))?;
                            }
                            cluster.id
                        }
                        _ => {
                            let blob = vec_to_blob(v);
                            tx.execute(
                                "INSERT INTO face_clusters (cover_path, proto, count, created_at)
                                 VALUES (NULL, ?1, 1, ?2)",
                                params![blob, now],
                            )
                            .map_err(|e| format!("新建人脸簇失败: {e}"))?;
                            let id = tx.last_insert_rowid();
                            // 封面用人脸裁切图，而不是整张照片：人物册一眼就能认人
                            let cover = face::save_face_cover(&face_covers_dir(), &format!("c{id}"), crop);
                            if let Some(p) = &cover {
                                let _ = tx.execute(
                                    "UPDATE face_clusters SET cover_path = ?1 WHERE id = ?2",
                                    params![p, id],
                                );
                            }
                            clusters.push(ClusterState {
                                id,
                                proto: v.clone(),
                                count: 1,
                                person_id: None,
                                exemplars: vec![v.clone()],
                            });
                            id
                        }
                    };
                    let vblob = vec_to_blob(v);
                    tx.execute(
                        "INSERT INTO faces (path, x1, y1, x2, y2, score, vec, cluster_id, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        params![path, f.x1, f.y1, f.x2, f.y2, f.score, vblob, cluster_id, now],
                    )
                    .map_err(|e| format!("写入人脸失败: {e}"))?;
                    face_count += 1;
                }
                tx.execute(
                    "INSERT OR REPLACE INTO face_scanned (path, mtime) VALUES (?1, ?2)",
                    params![path, mtime],
                )
                .map_err(|e| format!("记录扫描失败: {e}"))?;
                tx.commit().map_err(|e| format!("提交人脸失败: {e}"))?;
            } else {
                let db = conn.lock().map_err(|_| "smart.db 被占用".to_string())?;
                let _ = db.execute(
                    "INSERT OR REPLACE INTO face_scanned (path, mtime) VALUES (?1, ?2)",
                    params![path, mtime],
                );
            }
            done += 1;
            if done % 20 == 0 || done == total || last_emit.elapsed().as_secs() >= 1 {
                last_emit = std::time::Instant::now();
                progress(FaceProgress {
                    done,
                    total,
                    faces: face_count,
                    current: Path::new(path)
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default(),
                });
            }
        }

        // 版本盖章：往后追加的照片都基于同一套参数，可以放心混算
        let _ = self.set_setting(FACE_SCAN_VER_KEY, FACE_SCAN_VER);

        Ok(FaceSummary {
            total,
            skipped,
            faces: face_count,
            clusters: clusters.len(),
            cancelled,
        })
    }

    pub fn face_clusters(&self) -> Vec<FaceClusterRow> {
        let Some(db) = self.conn.as_ref().and_then(|c| c.lock().ok()) else {
            return Vec::new();
        };
        let Ok(mut stmt) = db.prepare(
            "SELECT id, count, cover_path, person_id, ignored FROM face_clusters
             ORDER BY count DESC, id",
        ) else {
            return Vec::new();
        };
        match stmt.query_map([], |r| {
            Ok(FaceClusterRow {
                id: r.get(0)?,
                count: r.get(1)?,
                cover_path: r.get(2)?,
                person_id: r.get(3)?,
                ignored: r.get::<_, i64>(4)? != 0,
            })
        }) {
            Ok(it) => it.flatten().collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn face_cluster_photos(&self, id: i64) -> Vec<String> {
        let Some(db) = self.conn.as_ref().and_then(|c| c.lock().ok()) else {
            return Vec::new();
        };
        let Ok(mut stmt) = db.prepare(
            "SELECT path, MAX(score) AS s FROM faces WHERE cluster_id = ?1
             GROUP BY path ORDER BY s DESC",
        ) else {
            return Vec::new();
        };
        match stmt.query_map(params![id], |r| r.get::<_, String>(0)) {
            Ok(it) => it.flatten().collect(),
            Err(_) => Vec::new(),
        }
    }

    /// 已扫描照片数与簇总数：人物段的「已扫 N 张」与后台状态用，不做全库遍历。
    pub fn face_stats(&self) -> (i64, i64) {
        let Some(db) = self.conn.as_ref().and_then(|c| c.lock().ok()) else {
            return (0, 0);
        };
        let scanned = db
            .query_row("SELECT COUNT(*) FROM face_scanned", [], |r| r.get(0))
            .unwrap_or(0);
        let clusters = db
            .query_row("SELECT COUNT(*) FROM face_clusters", [], |r| r.get(0))
            .unwrap_or(0);
        (scanned, clusters)
    }

    /// 一键清除人脸数据（生物特征）：faces / face_clusters / face_scanned 全清，
    /// 并删掉由人脸自动写入的 `album_members(source='face')`。
    /// **人物册本身与手动关联保留**——那是用户自己的目录，不该被 AI 清理连带删掉。
    /// 返回删除的人脸条数。
    pub fn clear_faces(&self) -> Result<usize, String> {
        let mut db = self.conn.as_ref().and_then(|c| c.lock().ok()).ok_or("smart.db 不可用")?;
        let tx = db.transaction().map_err(|e| format!("清除失败: {e}"))?;
        let faces: usize = tx
            .query_row("SELECT COUNT(*) FROM faces", [], |r| r.get::<_, i64>(0))
            .unwrap_or(0) as usize;
        tx.execute("DELETE FROM faces", [])
            .map_err(|e| format!("清除人脸失败: {e}"))?;
        tx.execute("DELETE FROM face_clusters", [])
            .map_err(|e| format!("清除人脸簇失败: {e}"))?;
        tx.execute("DELETE FROM face_scanned", [])
            .map_err(|e| format!("清除扫描记录失败: {e}"))?;
        tx.execute("DELETE FROM album_members WHERE source = 'face'", [])
            .map_err(|e| format!("清除人物册成员失败: {e}"))?;
        tx.commit().map_err(|e| format!("清除失败: {e}"))?;
        drop(db);
        // 人脸封面（裁切图）一起删：它们本身就是人脸图像，属于要清除的范围
        let _ = std::fs::remove_dir_all(face_covers_dir());
        Ok(faces)
    }

    /// 给簇命名：复用/创建同名人物册，绑定簇并把成员写入 `album_members(source='face')`。
    /// 同名已存在则直接合并到该人物，不重复建册。
    pub fn name_face_cluster(&self, id: i64, name: &str) -> Result<PersonRow, String> {
        let name = person_name(name)?;
        let mut db = self.person_conn()?;
        let tx = db
            .transaction()
            .map_err(|e| format!("命名失败: {e}"))?;
        let now = chrono::Local::now()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let existing: Option<i64> = tx
            .query_row(
                "SELECT id FROM albums WHERE kind = 'person' AND name = ?1 COLLATE NOCASE",
                params![name],
                |r| r.get(0),
            )
            .ok();
        let person_id = match existing {
            Some(pid) => pid,
            None => {
                tx.execute(
                    "INSERT INTO albums (kind, name, created_at) VALUES ('person', ?1, ?2)",
                    params![name, now],
                )
                .map_err(|e| format!("新建人物失败: {e}"))?;
                tx.last_insert_rowid()
            }
        };
        tx.execute(
            "UPDATE face_clusters SET person_id = ?1 WHERE id = ?2",
            params![person_id, id],
        )
        .map_err(|e| format!("绑定人脸簇失败: {e}"))?;
        tx.execute(
            "INSERT OR IGNORE INTO album_members (album_id, path, confidence, source)
             SELECT ?1, path, score, 'face' FROM faces WHERE cluster_id = ?2",
            params![person_id, id],
        )
        .map_err(|e| format!("写入人物册失败: {e}"))?;
        tx.execute(
            "UPDATE albums SET cover_path = COALESCE(cover_path,
                 (SELECT cover_path FROM face_clusters WHERE id = ?1)) WHERE id = ?2",
            params![id, person_id],
        )
        .map_err(|e| format!("设置封面失败: {e}"))?;
        tx.commit().map_err(|e| format!("命名失败: {e}"))?;
        drop(db);
        self.people()?
            .into_iter()
            .find(|p| p.id == person_id)
            .ok_or_else(|| "人物已创建但读取失败，请刷新".to_string())
    }

    pub fn ignore_face_cluster(&self, id: i64) -> Result<(), String> {
        let db = self.person_conn()?;
        let n = db
            .execute(
                "UPDATE face_clusters SET ignored = 1 WHERE id = ?1",
                params![id],
            )
            .map_err(|e| format!("忽略失败: {e}"))?;
        if n == 0 {
            return Err("人脸簇不存在".into());
        }
        Ok(())
    }

    // ── 人物相册：手动建册（docs §35）────────────────────────────────────
    // 复用 albums(kind='person') + album_members(source='user')，零新表。
    // 所有写操作在 DB 不可用 / 被占用 / 目标不存在时返回 Err，绝不静默成功。

    fn person_conn(&self) -> Result<std::sync::MutexGuard<'_, Connection>, String> {
        let conn = self
            .conn
            .as_ref()
            .ok_or_else(|| "smart.db 不可用，人物相册数据未保存".to_string())?;
        conn.lock()
            .map_err(|_| "smart.db 被占用，请稍后重试".to_string())
    }

    /// 人物存在性校验：读照片 / 关联 / 取消关联前都要过这一关，
    /// 「人物已被删除」不能伪装成「这个人物里没有照片」。
    fn ensure_person(db: &Connection, id: i64) -> Result<(), String> {
        let exists: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM albums WHERE id = ?1 AND kind = 'person')",
                params![id],
                |r| r.get(0),
            )
            .map_err(|e| format!("读取人物失败: {e}"))?;
        if !exists {
            return Err("人物不存在或已被删除，请刷新后重试".into());
        }
        Ok(())
    }

    pub fn people(&self) -> Result<Vec<PersonRow>, String> {
        let db = self.person_conn()?;
        let sql = person_select("WHERE a.kind = 'person' ORDER BY a.id");
        let mut stmt = db
            .prepare(&sql)
            .map_err(|e| format!("读取人物失败: {e}"))?;
        stmt.query_map([], map_person_row)
            .map_err(|e| format!("读取人物失败: {e}"))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| format!("读取人物失败: {e}"))
    }

    /// 单张照片已关联的人物（图库信息栏用）。只读展示，失败返回空而非打断信息栏。
    pub fn photo_people(&self, path: &str) -> Vec<PersonRow> {
        let Ok(db) = self.person_conn() else {
            return Vec::new();
        };
        let sql = person_select(
            "JOIN album_members pm ON pm.album_id = a.id
             WHERE a.kind = 'person' AND pm.path = ?1 ORDER BY a.id",
        );
        let Ok(mut stmt) = db.prepare(&sql) else {
            return Vec::new();
        };
        stmt.query_map(params![path], map_person_row)
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    pub fn create_person(&self, name: &str) -> Result<PersonRow, String> {
        let name = person_name(name)?;
        let db = self.person_conn()?;
        if person_name_exists(&db, name, None)? {
            return Err(format!("已存在同名人物「{name}」"));
        }
        let now = chrono::Local::now()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        db.execute(
            "INSERT INTO albums (kind, name, created_at) VALUES ('person', ?1, ?2)",
            params![name, now],
        )
        .map_err(|e| format!("新建人物失败: {e}"))?;
        Ok(PersonRow {
            id: db.last_insert_rowid(),
            name: name.to_string(),
            cover_path: None,
            count: 0,
            created_at: now,
        })
    }

    pub fn rename_person(&self, id: i64, name: &str) -> Result<(), String> {
        let name = person_name(name)?;
        let db = self.person_conn()?;
        Self::ensure_person(&db, id)?;
        if person_name_exists(&db, name, Some(id))? {
            return Err(format!("已存在同名人物「{name}」"));
        }
        let n = db
            .execute(
                "UPDATE albums SET name = ?1 WHERE id = ?2 AND kind = 'person'",
                params![name, id],
            )
            .map_err(|e| format!("重命名失败: {e}"))?;
        if n == 0 {
            return Err("人物不存在或已被删除".into());
        }
        Ok(())
    }

    /// 删除人物及其全部关联（单事务）。**只删索引，照片文件零改动**。
    pub fn delete_person(&self, id: i64) -> Result<usize, String> {
        let mut db = self.person_conn()?;
        let tx = db.transaction().map_err(|e| format!("删除人物失败: {e}"))?;
        Self::ensure_person(&tx, id)?;
        let n = tx
            .execute("DELETE FROM album_members WHERE album_id = ?1", params![id])
            .map_err(|e| format!("删除人物失败: {e}"))?;
        tx.execute("DELETE FROM albums WHERE id = ?1 AND kind = 'person'", params![id])
            .map_err(|e| format!("删除人物失败: {e}"))?;
        tx.commit().map_err(|e| format!("删除人物失败: {e}"))?;
        Ok(n)
    }

    pub fn person_photos(&self, id: i64) -> Result<Vec<String>, String> {
        let db = self.person_conn()?;
        Self::ensure_person(&db, id)?;
        let mut stmt = db
            .prepare("SELECT path FROM album_members WHERE album_id = ?1 ORDER BY path")
            .map_err(|e| format!("读取人物照片失败: {e}"))?;
        stmt.query_map(params![id], |r| r.get::<_, String>(0))
            .map_err(|e| format!("读取人物照片失败: {e}"))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| format!("读取人物照片失败: {e}"))
    }

    /// 批量关联：`INSERT OR IGNORE` 幂等。同批重复 / 已关联 / 文件已不在库中
    /// 都如实计数，不报错也不重复写入（docs §35.5 重复操作）。
    pub fn person_link(&self, id: i64, paths: &[String]) -> Result<LinkReport, String> {
        let mut db = self.person_conn()?;
        Self::ensure_person(&db, id)?;
        let tx = db.transaction().map_err(|e| format!("关联失败: {e}"))?;
        let mut report = LinkReport::default();
        let mut seen: HashSet<String> = HashSet::new();
        for raw in paths {
            let p = raw.trim();
            if p.is_empty() || !Path::new(p).is_file() {
                report.skipped += 1;
                continue;
            }
            if !seen.insert(p.to_string()) {
                report.duplicates += 1;
                continue;
            }
            let n = tx
                .execute(
                    "INSERT OR IGNORE INTO album_members (album_id, path, confidence, source)
                     VALUES (?1, ?2, NULL, 'user')",
                    params![id, p],
                )
                .map_err(|e| format!("关联失败: {e}"))?;
            if n > 0 {
                report.added += 1;
            } else {
                report.duplicates += 1;
            }
        }
        tx.commit().map_err(|e| format!("关联失败: {e}"))?;
        Ok(report)
    }

    /// 批量取消关联。删到封面时清掉 `cover_path`，让列表回退到剩余成员首图，
    /// 不留下指向已移除照片的空封面卡。
    pub fn person_unlink(&self, id: i64, paths: &[String]) -> Result<LinkReport, String> {
        let mut db = self.person_conn()?;
        Self::ensure_person(&db, id)?;
        let tx = db.transaction().map_err(|e| format!("取消关联失败: {e}"))?;
        let mut report = LinkReport::default();
        let mut seen: HashSet<String> = HashSet::new();
        for raw in paths {
            let p = raw.trim();
            if p.is_empty() || !seen.insert(p.to_string()) {
                continue;
            }
            let n = tx
                .execute(
                    "DELETE FROM album_members WHERE album_id = ?1 AND path = ?2",
                    params![id, p],
                )
                .map_err(|e| format!("取消关联失败: {e}"))?;
            if n > 0 {
                report.removed += 1;
            } else {
                report.not_linked += 1;
            }
        }
        tx.execute(
            "UPDATE albums SET cover_path = NULL
             WHERE id = ?1 AND kind = 'person' AND cover_path IS NOT NULL
               AND cover_path NOT IN (SELECT path FROM album_members WHERE album_id = ?1)",
            params![id],
        )
        .map_err(|e| format!("取消关联失败: {e}"))?;
        tx.commit().map_err(|e| format!("取消关联失败: {e}"))?;
        Ok(report)
    }

    pub fn set_person_cover(&self, id: i64, path: &str) -> Result<(), String> {
        let db = self.person_conn()?;
        Self::ensure_person(&db, id)?;
        let is_member: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM album_members WHERE album_id = ?1 AND path = ?2)",
                params![id, path],
                |r| r.get(0),
            )
            .map_err(|e| format!("设置封面失败: {e}"))?;
        if !is_member {
            return Err("这张照片不在该人物相册中，无法设为封面".into());
        }
        db.execute(
            "UPDATE albums SET cover_path = ?1 WHERE id = ?2 AND kind = 'person'",
            params![path, id],
        )
        .map_err(|e| format!("设置封面失败: {e}"))?;
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

// ── 下载（docs §38.4）：只由用户主动点击触发，不自动、不后台 ────────────

fn begin_download(key: &str) -> Result<Arc<AtomicBool>, String> {
    let mut map = downloads().lock().map_err(|_| "下载状态被占用".to_string())?;
    if map.contains_key(key) {
        return Err("该模型正在下载".into());
    }
    let flag = Arc::new(AtomicBool::new(false));
    map.insert(key.to_string(), flag.clone());
    Ok(flag)
}

fn end_download(key: &str) {
    if let Ok(mut map) = downloads().lock() {
        map.remove(key);
    }
}

fn request_cancel(key: &str) -> bool {
    downloads()
        .lock()
        .ok()
        .and_then(|map| map.get(key).cloned())
        .map(|flag| {
            flag.store(true, Ordering::Relaxed);
            true
        })
        .unwrap_or(false)
}

/// 逐文件下载：已存在且大小正确 → 跳过（支持「手动放入 + 补下其余」）；
/// `*.part` 流式写入 → 大小 / 哈希校验 → 原子改名。
/// 失败保留已完成文件，返回具体文件名 + 原因；绝不把半成品当成品。
fn download_files(
    key: &str,
    spec: &ModelSpec,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(DownloadProgress),
) -> Result<Vec<DownloadedFile>, String> {
    let root = models_dir();
    std::fs::create_dir_all(&root).map_err(|e| format!("创建模型目录失败: {e}"))?;
    let file_total = spec.files.len();
    let mut done_files = Vec::new();
    // 下载专用 agent：**不能用整体 timeout** —— 190MB 在慢速网络下必然超过 30s，
    // 曾因此出现「小文件成功、大文件报 os error 10060」。这里只限制连接与两次读之间的空闲。
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(15))
        .timeout_read(std::time::Duration::from_secs(60))
        .build();

    for (idx, f) in spec.files.iter().enumerate() {
        let target = root.join(f.path);
        if file_ready(&target, f) {
            let bytes = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
            progress(DownloadProgress {
                key: key.to_string(),
                file: f.path.to_string(),
                file_index: idx,
                file_total,
                bytes_done: bytes,
                bytes_total: bytes,
                done: true,
            });
            done_files.push(DownloadedFile {
                path: f.path.to_string(),
                sha256: String::new(),
                bytes,
            });
            continue;
        }
        if f.url.is_empty() {
            return Err(format!("{} 缺少下载地址，请手动放入模型目录", f.path));
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
        }
        if cancel.load(Ordering::Relaxed) {
            return Err("已取消下载".into());
        }

        let resp = agent.get(f.url).call().map_err(|e| match e {
            ureq::Error::Status(code, _) => {
                format!("{} 下载失败：服务器返回 HTTP {code}", f.path)
            }
            other => format!("{} 下载失败：{other}（请检查网络或代理）", f.path),
        })?;
        let bytes_total = resp
            .header("Content-Length")
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(f.bytes);
        let mut reader = resp.into_reader();
        let part = target.with_file_name(format!(
            "{}.part",
            target
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("model")
        ));
        let mut out =
            std::fs::File::create(&part).map_err(|e| format!("写入 {} 失败: {e}", f.path))?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        let mut done = 0u64;
        let mut last_push = 0u64;
        loop {
            if cancel.load(Ordering::Relaxed) {
                drop(out);
                return Err("已取消下载".into());
            }
            let n = reader
                .read(&mut buf)
                .map_err(|e| format!("读取 {} 失败: {e}", f.path))?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])
                .map_err(|e| format!("写入 {} 失败: {e}", f.path))?;
            hasher.update(&buf[..n]);
            done += n as u64;
            // 每 1MB 推一次进度，避免 190MB 产生上千次 IPC 拖慢下载
            if done - last_push >= 1024 * 1024 {
                last_push = done;
                progress(DownloadProgress {
                    key: key.to_string(),
                    file: f.path.to_string(),
                    file_index: idx,
                    file_total,
                    bytes_done: done,
                    bytes_total,
                    done: false,
                });
            }
        }
        out.flush()
            .map_err(|e| format!("写入 {} 失败: {e}", f.path))?;
        drop(out);

        if done == 0 {
            let _ = std::fs::remove_file(&part);
            return Err(format!("{} 下载内容为空", f.path));
        }
        if f.bytes > 0 && done != f.bytes {
            let _ = std::fs::remove_file(&part);
            return Err(format!(
                "{} 大小不符：期望 {} 字节，实际 {} 字节",
                f.path, f.bytes, done
            ));
        }
        let sha = format!("{:x}", hasher.finalize());
        if !f.sha256.is_empty() && !sha.eq_ignore_ascii_case(f.sha256) {
            let _ = std::fs::remove_file(&part);
            return Err(format!("{} 校验失败：哈希与清单不一致", f.path));
        }
        std::fs::rename(&part, &target).map_err(|e| format!("保存 {} 失败: {e}", f.path))?;
        progress(DownloadProgress {
            key: key.to_string(),
            file: f.path.to_string(),
            file_index: idx,
            file_total,
            bytes_done: done,
            bytes_total: done.max(bytes_total),
            done: true,
        });
        done_files.push(DownloadedFile {
            path: f.path.to_string(),
            sha256: sha,
            bytes: done,
        });
    }
    Ok(done_files)
}

/// 门禁 + 两级状态：模型文件是否齐全（ready）与索引是否可用（index）分开报。
/// 只读文件系统与 smart.db，成本可忽略；不做缓存，手动放入后立刻可见。
#[tauri::command]
pub fn smart_model_status(state: State<'_, AppState>) -> Vec<ModelStatus> {
    let root = models_dir();
    MODELS
        .iter()
        .map(|spec| {
            let hashes = state.smart.model_file_hashes(spec.key);
            let mut missing = Vec::new();
            let files: Vec<ModelFileStatus> = spec
                .files
                .iter()
                .map(|f| {
                    let actual_bytes = std::fs::metadata(root.join(f.path))
                        .map(|m| if m.is_file() { m.len() } else { 0 })
                        .unwrap_or(0);
                    let present = actual_bytes > 0 && (f.bytes == 0 || actual_bytes == f.bytes);
                    if !present {
                        missing.push(f.path.to_string());
                    }
                    ModelFileStatus {
                        path: f.path.to_string(),
                        bytes: f.bytes,
                        present,
                        actual_bytes,
                        sha256: hashes.get(f.path).cloned().unwrap_or_default(),
                    }
                })
                .collect();
            let ready = missing.is_empty();
            let source = if !ready {
                "未下载"
            } else if state.smart.model_source_kind(spec.key).as_deref() == Some("download") {
                "已下载"
            } else {
                "已放入目录"
            };
            ModelStatus {
                key: spec.key.to_string(),
                name: spec.name.to_string(),
                desc: spec.desc.to_string(),
                size_mb: spec.size_mb,
                license: spec.license.to_string(),
                version: spec.version.to_string(),
                ready,
                source: source.to_string(),
                missing,
                files,
                index: state.smart.index_info(spec),
            }
        })
        .collect()
}

/// 下载一个能力的全部文件；进度经 Channel 推送，可随时取消。
#[tauri::command]
pub async fn smart_download_model(
    state: State<'_, AppState>,
    key: String,
    on_progress: Channel<DownloadProgress>,
) -> Result<(), String> {
    let cancel = begin_download(&key)?;
    let key2 = key.clone();
    let joined = tauri::async_runtime::spawn_blocking(move || {
        let spec = find_spec(&key2)?;
        download_files(&key2, spec, &cancel, &mut |p| {
            let _ = on_progress.send(p);
        })
    })
    .await
    .map_err(|e| format!("下载线程异常: {e}"));
    end_download(&key);
    let files = joined?;
    let files = files?;
    state.smart.record_model_files(&key, &files)?;
    Ok(())
}

#[tauri::command]
pub fn smart_cancel_download(key: String) -> bool {
    request_cancel(&key)
}

/// 删除模型文件释放磁盘；返回释放字节数。索引保留（§17.15）。
#[tauri::command]
pub fn smart_delete_model(state: State<'_, AppState>, key: String) -> Result<u64, String> {
    state.smart.delete_model(&key)
}

fn index_cancel_slot() -> &'static Mutex<Option<Arc<AtomicBool>>> {
    static SLOT: OnceLock<Mutex<Option<Arc<AtomicBool>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// 启动全库增量索引：后台 blocking 线程，进度经 Channel，可随时取消。
#[tauri::command]
pub async fn smart_index_start(
    state: State<'_, AppState>,
    on_progress: Channel<IndexProgress>,
) -> Result<IndexSummary, String> {
    if !clip::model_present() {
        return Err("语义搜图模型未下载".into());
    }
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut guard = index_cancel_slot()
            .lock()
            .map_err(|_| "索引状态被占用".to_string())?;
        if guard.is_some() {
            return Err("索引任务已在运行".into());
        }
        *guard = Some(cancel.clone());
    }
    let cfg = state.config.lock().unwrap().clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        Smart::open().index_clip(&cfg.library_root, &cfg.original_dir, &cancel, |p| {
            let _ = on_progress.send(p);
        })
    })
    .await
    .map_err(|e| format!("索引线程异常: {e}"));
    if let Ok(mut guard) = index_cancel_slot().lock() {
        *guard = None;
    }
    result?
}

#[tauri::command]
pub fn smart_index_cancel() -> bool {
    index_cancel_slot()
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().cloned())
        .map(|c| {
            c.store(true, Ordering::Relaxed);
            true
        })
        .unwrap_or(false)
}

/// 语义搜索：返回按余弦相似度排序的照片路径。
#[tauri::command]
pub fn smart_search(
    state: State<'_, AppState>,
    query: String,
    limit: Option<usize>,
) -> Result<Vec<SearchHit>, String> {
    let q = query.trim();
    if q.is_empty() {
        return Err("请输入要搜索的内容".into());
    }
    state.smart.search_clip(q, limit.unwrap_or(60))
}

/// 主体分类列表（固定标签 + 数量 + 封面）。
#[tauri::command]
pub fn smart_things(state: State<'_, AppState>) -> Vec<ThingRow> {
    let rejects = state.meta.all_rejects();
    state.smart.things(&rejects)
}

#[tauri::command]
pub fn smart_thing_photos(state: State<'_, AppState>, label: String) -> Vec<String> {
    let rejects = state.meta.all_rejects();
    state.smart.thing_photos(&label, &rejects)
}

/// 用已有向量重算标签（不重新推理）；索引完成后可手动触发。
#[tauri::command]
pub fn smart_backfill_tags(state: State<'_, AppState>) -> Result<usize, String> {
    state.smart.backfill_tags()
}

fn face_cancel_slot() -> &'static Mutex<Option<Arc<AtomicBool>>> {
    static SLOT: OnceLock<Mutex<Option<Arc<AtomicBool>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// 启动一次全库人脸扫描：**立即返回**，进度与结果都走事件广播，
/// 这样后台自动扫描和手动扫描共用同一条通道，页面重进也能接上进度。
///   - `smart-face-progress`  FaceProgress
///   - `smart-face-done`      FaceSummary
///   - `smart-face-error`     String
fn spawn_face_scan(app: AppHandle, cfg: crate::models::AppConfig, delay_secs: u64) -> Result<(), String> {
    if !face::models_present() {
        return Err("人脸模型未下载".into());
    }
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut guard = face_cancel_slot()
            .lock()
            .map_err(|_| "扫描状态被占用".to_string())?;
        if guard.is_some() {
            return Err("人脸扫描已在运行".into());
        }
        *guard = Some(cancel.clone());
    }
    std::thread::spawn(move || {
        if delay_secs > 0 {
            // 分片睡眠：等待期间用户取消可立刻退出，不必睡满
            for _ in 0..delay_secs {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
        let emitted = |p: FaceProgress| {
            let _ = app.emit("smart-face-progress", p);
        };
        let result = if cancel.load(Ordering::Relaxed) {
            Ok(FaceSummary {
                total: 0,
                skipped: 0,
                faces: 0,
                clusters: 0,
                cancelled: true,
            })
        } else {
            Smart::open().scan_faces(&cfg.library_root, &cfg.original_dir, 0, &cancel, emitted)
        };
        if let Ok(mut guard) = face_cancel_slot().lock() {
            *guard = None;
        }
        match result {
            Ok(s) => {
                let _ = app.emit("smart-face-done", s);
            }
            Err(e) => {
                let _ = app.emit("smart-face-error", e);
            }
        }
    });
    Ok(())
}

/// 手动开始扫描（按钮）。立即返回，进度靠事件。
#[tauri::command]
pub fn smart_face_scan(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let cfg = state.config.lock().unwrap().clone();
    spawn_face_scan(app, cfg, 0)
}

/// 后台自动扫描：只在用户同意过、且模型就绪时才真的启动。
/// 返回 true = 本次已排入队列（含 20 秒缓冲），false = 条件不满足，静默跳过。
#[tauri::command]
pub fn smart_face_auto_start(app: AppHandle, state: State<'_, AppState>) -> Result<bool, String> {
    if state.smart.get_setting(FACE_AUTO_KEY).as_deref() != Some("on") {
        return Ok(false);
    }
    if !face::models_present() {
        return Ok(false);
    }
    // 旧结果需先清除：后台扫描不该在启动时刷一条红色错误，静默跳过，
    // 由人物段的琥珀提示条引导用户点「清除并重扫」。
    let (_, clusters) = state.smart.face_stats();
    if clusters > 0 && state.smart.get_setting(FACE_SCAN_VER_KEY).as_deref() != Some(FACE_SCAN_VER) {
        return Ok(false);
    }
    let cfg = state.config.lock().unwrap().clone();
    match spawn_face_scan(app, cfg, FACE_AUTO_DELAY_SECS) {
        Ok(()) => Ok(true),
        // 已在跑（用户先点了手动扫描）不算错误
        Err(e) if e.contains("已在运行") => Ok(false),
        Err(e) => Err(e),
    }
}

#[tauri::command]
pub fn smart_face_auto_scan_get(state: State<'_, AppState>) -> FaceAutoScan {
    let (scanned, clusters) = state.smart.face_stats();
    let ver = state.smart.get_setting(FACE_SCAN_VER_KEY);
    FaceAutoScan {
        consent: state
            .smart
            .get_setting(FACE_AUTO_KEY)
            .unwrap_or_else(|| "ask".to_string()),
        model_ready: face::models_present(),
        running: face_cancel_slot()
            .lock()
            .map(|g| g.is_some())
            .unwrap_or(false),
        scanned,
        clusters,
        stale: clusters > 0 && ver.as_deref() != Some(FACE_SCAN_VER),
    }
}

/// 写入用户意愿：ask / on / off。`on` 只表示「以后可以后台扫」，不会在此处直接启动。
#[tauri::command]
pub fn smart_set_face_auto_scan(state: State<'_, AppState>, choice: String) -> Result<(), String> {
    let v = choice.trim();
    if !matches!(v, "ask" | "on" | "off") {
        return Err("自动扫描选项只能是 ask / on / off".into());
    }
    state.smart.set_setting(FACE_AUTO_KEY, v)
}

#[tauri::command]
pub fn smart_face_cancel() -> bool {
    face_cancel_slot()
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().cloned())
        .map(|c| {
            c.store(true, Ordering::Relaxed);
            true
        })
        .unwrap_or(false)
}

#[tauri::command]
pub fn smart_face_clusters(state: State<'_, AppState>) -> Vec<FaceClusterRow> {
    state.smart.face_clusters()
}

#[tauri::command]
pub fn smart_face_cluster_photos(state: State<'_, AppState>, id: i64) -> Vec<String> {
    state.smart.face_cluster_photos(id)
}

#[tauri::command]
pub fn smart_face_name(
    state: State<'_, AppState>,
    id: i64,
    name: String,
) -> Result<PersonRow, String> {
    state.smart.name_face_cluster(id, &name)
}

#[tauri::command]
pub fn smart_face_ignore(state: State<'_, AppState>, id: i64) -> Result<(), String> {
    state.smart.ignore_face_cluster(id)
}

/// 一键清除人脸数据（生物特征）。扫描中不允许清：否则会边删边写。
#[tauri::command]
pub fn smart_face_clear(state: State<'_, AppState>) -> Result<usize, String> {
    if face_cancel_slot()
        .lock()
        .map(|g| g.is_some())
        .unwrap_or(false)
    {
        return Err("扫描进行中，请先取消再清除".into());
    }
    state.smart.clear_faces()
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

// ── 人物相册命令（docs §35）─────────────────────────────────────────────
// 参数只用 id / name / paths，避开 snake_case → camelCase 的命名歧义。

#[tauri::command]
pub fn smart_people(state: State<'_, AppState>) -> Result<Vec<PersonRow>, String> {
    state.smart.people()
}

#[tauri::command]
pub fn smart_create_person(
    state: State<'_, AppState>,
    name: String,
) -> Result<PersonRow, String> {
    state.smart.create_person(&name)
}

#[tauri::command]
pub fn smart_rename_person(
    state: State<'_, AppState>,
    id: i64,
    name: String,
) -> Result<(), String> {
    state.smart.rename_person(id, &name)
}

#[tauri::command]
pub fn smart_delete_person(state: State<'_, AppState>, id: i64) -> Result<usize, String> {
    state.smart.delete_person(id)
}

#[tauri::command]
pub fn smart_person_photos(
    state: State<'_, AppState>,
    id: i64,
) -> Result<Vec<String>, String> {
    state.smart.person_photos(id)
}

#[tauri::command]
pub fn smart_person_link(
    state: State<'_, AppState>,
    id: i64,
    paths: Vec<String>,
) -> Result<LinkReport, String> {
    state.smart.person_link(id, &paths)
}

#[tauri::command]
pub fn smart_person_unlink(
    state: State<'_, AppState>,
    id: i64,
    paths: Vec<String>,
) -> Result<LinkReport, String> {
    state.smart.person_unlink(id, &paths)
}

#[tauri::command]
pub fn smart_set_person_cover(
    state: State<'_, AppState>,
    id: i64,
    path: String,
) -> Result<(), String> {
    state.smart.set_person_cover(id, &path)
}

#[tauri::command]
pub fn smart_photo_people(
    state: State<'_, AppState>,
    path: String,
) -> Vec<PersonRow> {
    state.smart.photo_people(&path)
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

    /// 只在显式要求时跑：用 OpenCV 官方合影验证「不同人」余弦必须明显低于同人。
    /// cargo test --lib probe_face_demo -- --ignored --nocapture
    #[test]
    #[ignore]
    fn probe_face_demo() {
        let p = Path::new("demo_face.jpg");
        if !p.is_file() {
            eprintln!("demo_face.jpg 不存在，跳过");
            return;
        }
        let img = image::open(p).expect("读取 demo 失败");
        let faces = face::with_faces(|e| e.detect(&img)).expect("检测失败");
        eprintln!("{}x{} 检测到 {} 张脸", img.width(), img.height(), faces.len());
        let mut vecs = Vec::new();
        for f in &faces {
            let (v, _) = face::with_faces(|e| e.embed(&img, f)).expect("编码失败");
            eprintln!(
                "  框 {:.0},{:.0}-{:.0},{:.0} score {:.2} kps {:?}",
                f.x1, f.y1, f.x2, f.y2, f.score, f.kps
            );
            vecs.push(v);
        }
        for i in 0..vecs.len() {
            for j in (i + 1)..vecs.len() {
                let c: f32 = vecs[i].iter().zip(vecs[j].iter()).map(|(a, b)| a * b).sum();
                eprintln!("  cos(脸{}, 脸{}) = {:.4}", i + 1, j + 1, c);
            }
        }
    }

    /// 只在显式要求时跑：取 8 张有脸的照片，打印两两余弦，判断聚类阈值是否合理。
    /// cargo test --lib probe_face_pairwise -- --ignored --nocapture
    #[test]
    #[ignore]
    fn probe_face_pairwise() {
        let root = Path::new(r"D:\媒体\图片\原图");
        if !root.is_dir() {
            eprintln!("fixture 不存在，跳过");
            return;
        }
        let mut all = Vec::new();
        for e in WalkDir::new(root).into_iter().filter_map(Result::ok) {
            let p = e.path();
            if p.is_file()
                && matches!(
                    p.extension()
                        .and_then(|x| x.to_str())
                        .map(|s| s.to_ascii_lowercase())
                        .as_deref(),
                    Some("jpg") | Some("jpeg")
                )
            {
                all.push(p.to_path_buf());
            }
        }
        all.sort();
        let step = (all.len() / 200).max(1);
        let mut items: Vec<(String, Vec<f32>)> = Vec::new();
        for p in all.iter().step_by(step) {
            if items.len() >= 8 {
                break;
            }
            let Ok(meta) = std::fs::metadata(p) else { continue };
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let Some(bytes) = crate::thumbs::lookup(p, mtime, meta.len())
                .or_else(|| crate::thumbs::create(p, mtime, meta.len()))
            else {
                continue;
            };
            let Ok(img) = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg)
            else {
                continue;
            };
            let Ok(faces) = face::with_faces(|e| e.detect(&img)) else {
                continue;
            };
            if let Some(f) = faces.first() {
                if let Ok((v, _)) = face::with_faces(|e| e.embed(&img, f)) {
                    items.push((p.file_name().unwrap().to_string_lossy().to_string(), v));
                }
            }
        }
        eprintln!("收集 {} 张有脸照片", items.len());
        for i in 0..items.len() {
            for j in (i + 1)..items.len() {
                let c: f32 = items[i]
                    .1
                    .iter()
                    .zip(items[j].1.iter())
                    .map(|(a, b)| a * b)
                    .sum();
                eprintln!("{:.4}  {}  {}", c, items[i].0, items[j].0);
            }
        }
    }

    /// 只在显式要求时跑：扫描 40 张验证「检测 → 聚类 → 写库」，结束后清理测试数据。
    /// cargo test --lib probe_face_scan_40 -- --ignored --nocapture
    #[test]
    #[ignore]
    fn probe_face_scan_40() {
        let smart = Smart::open();
        let cancel = AtomicBool::new(false);
        let summary = smart
            .scan_faces(r"D:\媒体\图片", "原图", 40, &cancel, |p| {
                if p.done % 20 == 0 {
                    eprintln!("  {}/{} 已检出 {} 脸", p.done, p.total, p.faces);
                }
            })
            .expect("扫描失败");
        eprintln!("{summary:?}");
        for c in smart.face_clusters().iter().take(8) {
            eprintln!(
                "  cluster {} | {} 张 | person={:?} | cover={:?}",
                c.id, c.count, c.person_id, c.cover_path
            );
        }
        // 清理测试数据，避免污染正式扫描
        if let Some(conn) = &smart.conn {
            if let Ok(db) = conn.lock() {
                let _ = db.execute("DELETE FROM faces", []);
                let _ = db.execute("DELETE FROM face_clusters", []);
                let _ = db.execute("DELETE FROM face_scanned", []);
            }
        }
    }

    /// 只在显式要求时跑：用 256px 缩略图检测已知有脸的 4 张，验证小图是否可用。
    /// cargo test --lib probe_face_256 -- --ignored --nocapture
    #[test]
    #[ignore]
    fn probe_face_256() {
        let names = ["P1163343.JPG", "P1244154.JPG", "P1244355.JPG", "P1244714.JPG"];
        let root = Path::new(r"D:\媒体\图片\原图");
        if !root.is_dir() {
            eprintln!("fixture 不存在，跳过");
            return;
        }
        let mut vecs: Vec<(String, Vec<f32>)> = Vec::new();
        for e in WalkDir::new(root).into_iter().filter_map(Result::ok) {
            let p = e.path();
            let Some(name) = p.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if !names.contains(&name) {
                continue;
            }
            let Ok(meta) = std::fs::metadata(p) else { continue };
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let Some(bytes) = crate::thumbs::lookup(p, mtime, meta.len())
                .or_else(|| crate::thumbs::create(p, mtime, meta.len()))
            else {
                eprintln!("{name} 缩略图失败");
                continue;
            };
            let Ok(img) = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg)
            else {
                continue;
            };
            let t = std::time::Instant::now();
            let faces = match face::with_faces(|eng| eng.detect(&img)) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("检测失败: {e}");
                    return;
                }
            };
            eprintln!(
                "{name} | {}x{} | {} 脸 | top {:?} | {:?}",
                img.width(),
                img.height(),
                faces.len(),
                faces.first().map(|f| f.score),
                t.elapsed()
            );
            if let Some(f) = faces.first() {
                if let Ok((v, _)) = face::with_faces(|eng| eng.embed(&img, f)) {
                    vecs.push((name.to_string(), v));
                }
            }
        }
        for i in 1..vecs.len() {
            let c: f32 = vecs[0]
                .1
                .iter()
                .zip(vecs[i].1.iter())
                .map(|(a, b)| a * b)
                .sum();
            eprintln!("cos({}, {}) = {:.4}", vecs[0].0, vecs[i].0, c);
        }
    }

    /// 只在显式要求时跑：只读统计「新判定」在全库的分布，不写库。
    /// cargo test --lib probe_thing_dist -- --ignored --nocapture
    #[test]
    #[ignore]
    fn probe_thing_dist() {
        use std::collections::HashMap;
        let smart = Smart::open();
        let Some(conn) = &smart.conn else {
            eprintln!("smart.db 不可用");
            return;
        };
        let rows: Vec<Vec<u8>> = {
            let db = match conn.lock() {
                Ok(d) => d,
                Err(_) => return,
            };
            let Ok(mut stmt) = db.prepare("SELECT clip_vec FROM media WHERE clip_vec IS NOT NULL")
            else {
                return;
            };
            stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))
                .map(|it| it.flatten().collect())
                .unwrap_or_default()
        };
        let labels = clip::thing_vectors().expect("标签向量失败");
        let thresholds = [0.55f32, 0.65, 0.75, 0.85];
        let mut per: Vec<HashMap<&str, usize>> = vec![HashMap::new(); thresholds.len()];
        let mut conf_hist = [0usize; 5]; // <0.3 <0.5 <0.7 <0.85 >=0.85
        for blob in &rows {
            let v = blob_to_vec(blob);
            if v.len() != 512 {
                continue;
            }
            let mut scored: Vec<(&str, f32)> = labels
                .iter()
                .map(|(l, tv)| (*l, v.iter().zip(tv.iter()).map(|(a, b)| a * b).sum::<f32>()))
                .collect();
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            let conf = clip::thing_confidence(&scored);
            let (label, cos) = scored[0];
            let bucket = if conf < 0.3 {
                0
            } else if conf < 0.5 {
                1
            } else if conf < 0.7 {
                2
            } else if conf < 0.85 {
                3
            } else {
                4
            };
            conf_hist[bucket] += 1;
            if cos < clip::THING_MIN_COS {
                continue;
            }
            for (i, t) in thresholds.iter().enumerate() {
                if conf >= *t {
                    *per[i].entry(label).or_default() += 1;
                }
            }
        }
        eprintln!("总数 {} · conf 分布 <0.3/0.5/0.7/0.85/>=0.85 = {conf_hist:?}", rows.len());
        for (i, t) in thresholds.iter().enumerate() {
            let total: usize = per[i].values().sum();
            let mut list: Vec<_> = per[i].iter().collect();
            list.sort_by(|a, b| b.1.cmp(a.1));
            eprintln!(
                "conf>={t}: {total} 张（{:.1}%）· {}",
                total as f64 * 100.0 / rows.len().max(1) as f64,
                list.iter()
                    .map(|(l, n)| format!("{l}{n}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
    }

    /// 只在显式要求时跑：用 9 张「已人工确认」的样本（3 张食物 / 6 张非食物）
    /// 对比「纯标签」与「模板提示」两种打分，并看 softmax 置信度能否分开两者。
    /// cargo test --lib probe_food_confusion -- --ignored --nocapture
    #[test]
    #[ignore]
    fn probe_food_confusion() {
        // (路径, 是否真的是食物)
        let cases: &[(&str, bool)] = &[
            (r"D:\媒体\图片\原图\2026\2026-06\PLUM5849.JPG", false), // 草原
            (r"D:\媒体\图片\原图\2026\2026-06\P1162998.JPG", false), // 雪山野餐
            (r"D:\媒体\图片\原图\2026\2026-08\PLUM5973.JPG", true),  // 蛋糕
            (r"D:\媒体\图片\原图\2026\2026-10\PLUM6284.JPG", false), // 古镇街
            (r"D:\媒体\图片\原图\2025\2025-08\PLUM4316.JPG", false), // 云
            (r"D:\媒体\图片\原图\2026\2026-05\P1151826.JPG", true),  // 冰品
            (r"D:\媒体\图片\原图\2026\2026-10\P1255161.JPG", true),  // 桌上餐食
            (r"D:\媒体\图片\原图\2026\2026-05\P1162239.JPG", false), // 海边人物
            (r"D:\媒体\图片\原图\2026\2026-08\P1244141.JPG", false), // 丹霞地貌
        ];
        // 两套提示文本
        let raw: Vec<(&'static str, Vec<f32>)> = clip::thing_vectors().expect("标签向量失败");
        let templated: Vec<(&'static str, Vec<f32>)> = clip::THING_LABELS
            .iter()
            .map(|l| {
                let v = clip::with_clip(|c| c.encode_text(&format!("一张{l}的照片"))).expect("编码失败");
                (*l, v)
            })
            .collect();

        let softmax_conf = |scores: &[f32]| -> f32 {
            let scale = 100.0f32;
            let max = scores.iter().cloned().fold(f32::MIN, f32::max);
            let sum: f32 = scores.iter().map(|s| ((s - max) * scale).exp()).sum();
            1.0 / sum
        };

        for (path, is_food) in cases {
            let p = Path::new(path);
            let Some(bytes) = std::fs::metadata(p)
                .ok()
                .and_then(|m| {
                    let mt = m
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    crate::thumbs::lookup(p, mt, m.len())
                        .or_else(|| crate::thumbs::create(p, mt, m.len()))
                })
            else {
                eprintln!("{} 无缩略图", p.display());
                continue;
            };
            let Ok(img) = clip::decode_thumb(&bytes) else { continue };
            let Ok(v) = clip::with_clip(|c| c.encode_image(&img)) else { continue };
            let name = p.file_name().unwrap().to_string_lossy();
            for (tag, labels) in [("raw", &raw), ("tpl", &templated)] {
                let mut scored: Vec<(&str, f32)> = labels
                    .iter()
                    .map(|(l, tv)| (*l, v.iter().zip(tv.iter()).map(|(a, b)| a * b).sum::<f32>()))
                    .collect();
                scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
                let conf = softmax_conf(&scored.iter().map(|s| s.1).collect::<Vec<_>>());
                eprintln!(
                    "{name} {tag} food={is_food} top3={} conf={conf:.3}",
                    scored
                        .iter()
                        .take(3)
                        .map(|(l, s)| format!("{l}:{s:.3}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
            }
        }
    }

    /// 只在显式要求时跑：拆开统计「解码原图」与「检测+编码」各自耗时，
    /// 用来判断扫描慢到底是慢在解码还是慢在推理。
    /// cargo test --lib probe_face_cost -- --ignored --nocapture
    #[test]
    #[ignore]
    fn probe_face_cost() {
        let root = Path::new(r"D:\媒体\图片\原图");
        if !root.is_dir() {
            eprintln!("fixture 不存在，跳过");
            return;
        }
        let mut files: Vec<PathBuf> = WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_type().is_file()
                    && matches!(
                        e.path()
                            .extension()
                            .and_then(|x| x.to_str())
                            .map(|s| s.to_ascii_lowercase())
                            .as_deref(),
                        Some("jpg") | Some("jpeg")
                    )
            })
            .map(|e| e.path().to_path_buf())
            .collect();
        files.sort();
        if files.len() > 20 {
            let step = files.len() / 20;
            files = files.into_iter().step_by(step).take(20).collect();
        }
        let (mut decode, mut detect, mut thumb_t) = (0f64, 0f64, 0f64);
        let mut n = 0;
        for p in &files {
            let t = std::time::Instant::now();
            let Ok(img) = face::load_for_scan(p, face::SCAN_MAX_SIDE) else { continue };
            decode += t.elapsed().as_secs_f64();

            let t = std::time::Instant::now();
            let faces = face::with_faces(|e| {
                let fs = e.detect(&img)?;
                for f in &fs {
                    let _ = e.embed(&img, f)?;
                }
                Ok(fs.len())
            })
            .unwrap_or(0);
            detect += t.elapsed().as_secs_f64();

            // 旧的缩略图路径做对照
            let Ok(meta) = std::fs::metadata(p) else { continue };
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let t = std::time::Instant::now();
            let _ = crate::thumbs::lookup(p, mtime, meta.len())
                .or_else(|| crate::thumbs::create(p, mtime, meta.len()));
            thumb_t += t.elapsed().as_secs_f64();

            eprintln!("{} 脸={}", p.file_name().unwrap().to_string_lossy(), faces);
            n += 1;
        }
        if n > 0 {
            eprintln!(
                "{} 张：解码原图 {:.0}ms/张 · 检测+编码 {:.0}ms/张 · 缩略图路径 {:.0}ms/张",
                n,
                decode * 1000.0 / n as f64,
                detect * 1000.0 / n as f64,
                thumb_t * 1000.0 / n as f64
            );
        }
    }

    /// 只在显式要求时跑：对比 256px 缩略图与 640px 原图两种扫描尺度下的同一性判定。
    /// 用真实库里的「同一个人 / 不同人」各一对做对照，验证 r2 尺度是否真的更分得开。
    /// cargo test --lib probe_face_scale -- --ignored --nocapture
    #[test]
    #[ignore]
    fn probe_face_scale() {
        // 前两张：同一簇（cluster 15）内不同场次 → 大概率同一人
        // 第三张：另一簇（cluster 10）→ 不同人
        let cases = [
            (r"D:\媒体\图片\原图\2025\2025-08\PLUM4183.JPG", "A1"),
            (r"D:\媒体\图片\原图\2026\2026-04\P1151681.JPG", "A2"),
            (r"D:\媒体\图片\原图\2024\2024-12\IMG_2517.JPG", "B1"),
        ];
        for (path, tag) in cases {
            let p = Path::new(path);
            if !p.is_file() {
                eprintln!("{tag} 不存在，跳过");
                continue;
            }
            // 256 缩略图（旧方案）
            let Ok(meta) = std::fs::metadata(p) else { continue };
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let thumb = crate::thumbs::lookup(p, mtime, meta.len())
                .or_else(|| crate::thumbs::create(p, mtime, meta.len()))
                .and_then(|b| image::load_from_memory_with_format(&b, image::ImageFormat::Jpeg).ok());
            // 640 原图（新方案）
            let big = face::load_for_scan(p, face::SCAN_MAX_SIDE).ok();

            let mut old = None;
            let mut new = None;
            if let Some(img) = &thumb {
                let t = std::time::Instant::now();
                old = face::with_faces(|e| {
                    let fs = e.detect(img)?;
                    let f = fs.first().ok_or("无脸")?;
                    e.embed(img, f).map(|(v, _)| v)
                })
                .ok();
                eprintln!("{tag} 256px {:?} 耗时 {:?}", old.is_some(), t.elapsed());
            }
            if let Some(img) = &big {
                let t = std::time::Instant::now();
                new = face::with_faces(|e| {
                    let fs = e.detect(img)?;
                    let f = fs.first().ok_or("无脸")?;
                    e.embed(img, f).map(|(v, _)| v)
                })
                .ok();
                eprintln!(
                    "{tag} 640px {:?} ({}x{}) 耗时 {:?}",
                    new.is_some(),
                    img.width(),
                    img.height(),
                    t.elapsed()
                );
            }
            if let (Some(o), Some(n)) = (&old, &new) {
                // 打印一下两者的模长，确认都归一化过
                let norm: f32 = n.iter().map(|x| x * x).sum();
                eprintln!("  {tag} new norm² = {norm:.4}");
                let _ = o;
            }
            // 存起来供两两比较
            SCALE_PROBE.lock().unwrap().push((tag.to_string(), old, new));
        }
        let items = SCALE_PROBE.lock().unwrap().clone();
        let cos = |a: &Vec<f32>, b: &Vec<f32>| -> f32 {
            a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
        };
        for i in 0..items.len() {
            for j in (i + 1)..items.len() {
                let (ta, oa, na) = &items[i];
                let (tb, ob, nb) = &items[j];
                if let (Some(a), Some(b)) = (oa, ob) {
                    eprintln!("256px cos({ta},{tb}) = {:.4}", cos(a, b));
                }
                if let (Some(a), Some(b)) = (na, nb) {
                    eprintln!("640px cos({ta},{tb}) = {:.4}", cos(a, b));
                }
            }
        }
        SCALE_PROBE.lock().unwrap().clear();
    }

    static SCALE_PROBE: std::sync::Mutex<Vec<(String, Option<Vec<f32>>, Option<Vec<f32>>)>> =
        std::sync::Mutex::new(Vec::new());

    /// 只在显式要求时跑：均匀取 30 张 JPG 验证 YuNet 检测框是否落在图内。
    /// cargo test --lib probe_face_detect -- --ignored --nocapture
    #[test]
    #[ignore]
    fn probe_face_detect() {
        use std::time::Instant;
        let root = Path::new(r"D:\媒体\图片\原图");
        if !root.is_dir() {
            eprintln!("fixture 不存在，跳过");
            return;
        }
        let mut all = Vec::new();
        for e in WalkDir::new(root).into_iter().filter_map(Result::ok) {
            let p = e.path();
            if p.is_file()
                && matches!(
                    p.extension()
                        .and_then(|x| x.to_str())
                        .map(|s| s.to_ascii_lowercase())
                        .as_deref(),
                    Some("jpg") | Some("jpeg")
                )
            {
                all.push(p.to_path_buf());
            }
        }
        all.sort();
        let step = (all.len() / 30).max(1);
        let mut total = 0usize;
        for p in all.iter().step_by(step).take(30) {
            let Ok(img) = image::open(p) else { continue };
            let t = Instant::now();
            let faces = match face::with_faces(|e| e.detect(&img)) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("检测失败: {e}");
                    return;
                }
            };
            total += faces.len();
            if let Some(f) = faces.first() {
                eprintln!(
                    "{} | {}x{} | {} 脸 | top {:.2} | box {:.0},{:.0}-{:.0},{:.0} | {:?}",
                    p.file_name().unwrap().to_string_lossy(),
                    img.width(),
                    img.height(),
                    faces.len(),
                    f.score,
                    f.x1,
                    f.y1,
                    f.x2,
                    f.y2,
                    t.elapsed()
                );
            }
        }
        eprintln!("30 张共检测到 {total} 张脸");
    }

    /// 只在显式要求时跑：用已有向量回填标签并打印各分类数量，验证阈值是否合理。
    /// cargo test --lib probe_backfill_tags -- --ignored --nocapture
    #[test]
    #[ignore]
    fn probe_backfill_tags() {
        let smart = Smart::open();
        let t = std::time::Instant::now();
        let n = smart.backfill_tags().expect("回填失败");
        eprintln!("写入 {n} 条标签，耗时 {:?}", t.elapsed());
        let things = smart.things(&HashSet::new());
        for row in things.iter().take(20) {
            eprintln!("  {:<6} {} 张  cover={:?}", row.label, row.count, row.cover_path);
        }
    }

    /// 只在显式要求时跑：真实库取 5 张 JPG，验证 CLIP 中文标签排序与单图耗时。
    /// cargo test --lib probe_clip_five_photos -- --ignored --nocapture
    #[test]
    #[ignore]
    fn probe_clip_five_photos() {
        use std::time::Instant;
        let root = Path::new(r"D:\媒体\图片\原图");
        if !root.is_dir() {
            eprintln!("fixture 不存在，跳过");
            return;
        }
        let mut all = Vec::new();
        for e in WalkDir::new(root).into_iter().filter_map(Result::ok) {
            let p = e.path();
            if p.is_file()
                && matches!(
                    p.extension()
                        .and_then(|x| x.to_str())
                        .map(|s| s.to_ascii_lowercase())
                        .as_deref(),
                    Some("jpg") | Some("jpeg")
                )
            {
                all.push(p.to_path_buf());
            }
        }
        all.sort();
        // 均匀取样，避免全是同一场景连拍（那样标签排序会看起来「没区分度」）
        let step = (all.len() / 6).max(1);
        let paths: Vec<_> = all.iter().step_by(step).take(6).cloned().collect();
        eprintln!("库内 JPG {} 张，取样 {}", all.len(), paths.len());
        let labels = ["人", "猫", "狗", "风景", "食物", "建筑", "车", "花", "夜景"];
        let t = Instant::now();
        let text_vecs: Vec<(&str, Vec<f32>)> = labels
            .iter()
            .map(|l| (*l, clip::with_clip(|c| c.encode_text(l)).expect("文本编码失败")))
            .collect();
        eprintln!("9 个标签编码耗时 {:?}", t.elapsed());

        let mut vecs: Vec<(String, Vec<f32>)> = Vec::new();
        for (idx, p) in paths.iter().enumerate() {
            let meta = std::fs::metadata(&p).expect("metadata");
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let Some(bytes) = crate::thumbs::lookup(&p, mtime, meta.len())
                .or_else(|| crate::thumbs::create(&p, mtime, meta.len()))
            else {
                eprintln!("缩略图失败 {}", p.display());
                continue;
            };
            let img = clip::decode_thumb(&bytes).expect("解码失败");
            let t = Instant::now();
            let v = clip::with_clip(|c| c.encode_image(&img)).expect("图像编码失败");
            let elapsed = t.elapsed();
            vecs.push((p.file_name().unwrap().to_string_lossy().to_string(), v.clone()));
            let mut scored: Vec<(&str, f32)> = text_vecs
                .iter()
                .map(|(l, tv)| (*l, v.iter().zip(tv.iter()).map(|(a, b)| a * b).sum()))
                .collect();
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            eprintln!(
                "{} | {:?} | top3 {:?}",
                p.file_name().unwrap().to_string_lossy(),
                elapsed,
                &scored[..3]
            );
            if idx == 0 {
                eprintln!("  （第 1 张作为基准，用于看不同场景之间的余弦是否退化）");
            }
        }
        for i in 1..vecs.len() {
            let c: f32 = vecs[0]
                .1
                .iter()
                .zip(vecs[i].1.iter())
                .map(|(a, b)| a * b)
                .sum();
            eprintln!("cos({}, {}) = {:.4}", vecs[0].0, vecs[i].0, c);
        }
    }

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

    // ── 人物相册（docs §35）───────────────────────────────────────────
    fn person_store(name: &str) -> (Smart, PathBuf) {
        let dir = std::env::temp_dir().join(format!("sdi-person-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
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
             CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;",
        )
        .unwrap();
        (Smart { conn: Some(Mutex::new(c)) }, dir)
    }

    fn shot(dir: &Path, name: &str) -> String {
        let p = dir.join(name);
        std::fs::write(&p, b"photo").unwrap();
        p.to_string_lossy().to_string()
    }

    #[test]
    fn person_crud_rejects_duplicate_name_and_cascades() {
        let (s, dir) = person_store("crud");
        assert!(s.people().unwrap().is_empty(), "初始无人物");
        let a = s.create_person("  阿星  ").unwrap();
        assert_eq!(a.name, "阿星", "名字 trim 后入库");
        assert!(s.create_person("阿星").is_err(), "重名必须拒绝");
        assert!(s.create_person("阿星 ").is_err(), "trim 后重名也要拒绝");
        assert!(s.create_person("   ").is_err());
        assert!(s.create_person(&"x".repeat(41)).is_err());

        s.create_person("小Q").unwrap();
        assert!(s.rename_person(a.id, "小Q").is_err(), "改成别人的名字也要拒绝");
        s.rename_person(a.id, "阿星2").unwrap();
        assert_eq!(s.people().unwrap()[0].name, "阿星2");

        let p = shot(&dir, "P1.JPG");
        s.person_link(a.id, &[p]).unwrap();
        assert_eq!(s.delete_person(a.id).unwrap(), 1, "级联删除返回关联数");
        assert_eq!(s.people().unwrap().len(), 1, "只删目标人物");
        assert!(s.person_photos(a.id).is_err(), "删后不能伪装成空相册");
        assert!(s.delete_person(a.id).is_err(), "重复删除要报错");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn person_link_is_idempotent_and_reports_duplicates() {
        let (s, dir) = person_store("link");
        let a = s.create_person("阿星").unwrap();
        let p1 = shot(&dir, "P1.JPG");
        let p2 = shot(&dir, "P1.RW2");

        let r = s.person_link(a.id, &[p1.clone(), p2.clone()]).unwrap();
        assert_eq!((r.added, r.duplicates, r.skipped), (2, 0, 0));

        // 已关联 / 同批重复 / 文件已不在库中：全部如实计数，不写孤儿行
        let missing = dir.join("GONE.JPG").to_string_lossy().to_string();
        let r = s.person_link(a.id, &[p1.clone(), p1.clone(), missing]).unwrap();
        assert_eq!(r.added, 0);
        assert_eq!(r.duplicates, 2);
        assert_eq!(r.skipped, 1);
        assert_eq!(s.person_photos(a.id).unwrap().len(), 2);
        assert_eq!(s.people().unwrap()[0].count, 2);
        assert_eq!(s.photo_people(&p1).len(), 1, "图库信息栏能查到关联人物");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn person_unlink_clears_stale_cover() {
        let (s, dir) = person_store("unlink");
        let a = s.create_person("阿星").unwrap();
        let p1 = shot(&dir, "A.JPG");
        let p2 = shot(&dir, "B.JPG");
        s.person_link(a.id, &[p1.clone(), p2.clone()]).unwrap();
        s.set_person_cover(a.id, &p1).unwrap();
        assert!(s.set_person_cover(a.id, "C.JPG").is_err(), "非成员不能设为封面");

        let r = s.person_unlink(a.id, &[p1.clone()]).unwrap();
        assert_eq!((r.removed, r.not_linked), (1, 0));
        let r = s.person_unlink(a.id, &[p1.clone()]).unwrap();
        assert_eq!((r.removed, r.not_linked), (0, 1), "重复取消如实回报");

        let row = &s.people().unwrap()[0];
        assert_eq!(row.count, 1);
        assert_eq!(row.cover_path.as_deref(), Some(p2.as_str()), "封面回退到剩余成员");
        assert!(s.photo_people(&p1).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn person_commands_report_missing_person_and_degraded_db() {
        let (s, dir) = person_store("errors");
        assert!(s.person_photos(999).is_err());
        assert!(s.person_link(999, &[]).is_err());
        assert!(s.person_unlink(999, &[]).is_err());
        assert!(s.rename_person(999, "x").is_err());
        assert!(s.delete_person(999).is_err());
        assert!(s.set_person_cover(999, "x").is_err());

        let dead = Smart { conn: None };
        assert!(dead.people().is_err(), "DB 不可用不能冒充空数据");
        assert!(dead.create_person("阿星").is_err());
        assert!(dead.person_photos(1).is_err());
        assert!(dead.person_link(1, &[]).is_err());
        assert!(dead.photo_people("x").is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
