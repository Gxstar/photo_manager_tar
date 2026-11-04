//! 单元测试：用真实媒体库文件做只读夹具（绝不写入库目录）。
//!
//! 夹具根目录通过环境变量 `SDI_FIXTURE_ROOT` 配置（指向媒体库根，只读），
//! 未配置或路径不存在时相关测试自动跳过——不依赖任何硬编码个人路径。

use crate::copier::copy_one;
use crate::meta::classify::DirFileMeta;
use crate::meta::photo::photo_shoot_time;
use crate::meta::video::video_shoot_time;
use crate::models::{FileKind, FileStatus};
use crate::scanner::{filename_date_time, filename_full_time};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// 夹具根目录：环境变量 SDI_FIXTURE_ROOT（媒体库根，测试只读）
fn fixture_root() -> Option<PathBuf> {
    std::env::var("SDI_FIXTURE_ROOT")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// 解析夹具相对路径；根未配置返回 None
fn fixture(rel: &str) -> Option<PathBuf> {
    fixture_root().map(|root| root.join(rel))
}

fn skip_msg() {
    eprintln!("跳过：未配置 SDI_FIXTURE_ROOT 或夹具不存在");
}

/// R-02：RW2 内嵌 APP1 解析，与同名 JPG 时间完全一致
#[test]
fn rw2_embedded_exif_matches_jpg() {
    let Some(fix_rw2) = fixture(r"原图\2026\2026-10\P1244659.RW2") else {
        skip_msg();
        return;
    };
    let Some(fix_jpg) = fixture(r"原图\2026\2026-10\P1244659.JPG") else {
        skip_msg();
        return;
    };
    if !fix_rw2.exists() || !fix_jpg.exists() {
        skip_msg();
        return;
    }
    let rw2 = photo_shoot_time(&fix_rw2).expect("RW2 应可解析");
    let jpg = photo_shoot_time(&fix_jpg).expect("JPG 应可解析");
    let (t_rw2, _) = rw2.expect("RW2 应含 DateTimeOriginal");
    let (t_jpg, _) = jpg.expect("JPG 应含 DateTimeOriginal");
    assert_eq!(
        t_rw2.format("%Y-%m-%d %H:%M:%S").to_string(),
        "2026-10-02 20:42:37"
    );
    assert_eq!(t_rw2, t_jpg, "RW2 与同名 JPG 时间必须一致");
}

/// R-02 配套：JPG 分辨率取自 EXIF sub-IFD 的 0xA002/0xA003（Context::Exif）。
/// 回归：曾用 Context::Tiff 查询导致 JPG 分辨率恒为「—」（RW2 走松下私有 tag 不受影响）。
#[test]
fn jpg_exif_resolution_parsed() {
    let Some(fix_jpg) = fixture(r"原图\2026\2026-10\PLUM6295.JPG") else {
        skip_msg();
        return;
    };
    if !fix_jpg.exists() {
        skip_msg();
        return;
    }
    let e = crate::meta::photo::photo_exif(&fix_jpg).expect("JPG 应可解析 EXIF");
    assert_eq!(e.width, Some(5184), "ExifImageWidth 应从 EXIF sub-IFD 取到");
    assert_eq!(e.height, Some(3888), "ExifImageHeight 应从 EXIF sub-IFD 取到");
}

/// R-03：MP4 mvhd 1904 纪元换算
#[test]
fn mp4_mvhd_uses_1904_epoch() {
    let Some(fix_mp4) = fixture(r"视频\2026\2026-10\P1244742.MP4") else {
        skip_msg();
        return;
    };
    if !fix_mp4.exists() {
        skip_msg();
        return;
    }
    let t = video_shoot_time(&fix_mp4).expect("MP4 应可解析");
    assert_eq!(
        t.format("%Y-%m-%d %H:%M:%S").to_string(),
        "2026-10-07 14:57:08"
    );
}

/// R-01 配套：文件名时间解析（IMG_ 全日期；全零拒绝；YYYYMMDD_ 前缀）
#[test]
fn filename_time_parsing() {
    let t = filename_full_time("IMG_20260329_123640.dng").expect("应解析成功");
    assert_eq!(
        t.format("%Y-%m-%d %H:%M:%S").to_string(),
        "2026-03-29 12:36:40"
    );

    // 全零拒绝
    assert!(filename_full_time("IMG_00000000_000000.dng").is_none());
    assert!(filename_date_time("00000000_Unknown_001.avif").is_none());
    // 非该模式的文件名
    assert!(filename_full_time("P1244649.JPG").is_none());
    assert!(filename_full_time("PLUM5103.JPG").is_none());

    // 低可靠前缀
    let d = filename_date_time("20260304_DC-S9_140.avif").expect("应解析出日期");
    assert_eq!(d.format("%Y-%m-%d").to_string(), "2026-03-04");
}

/// 落盘保护：create_new 撞已存在文件 → 拒绝且原文件分毫不动
#[test]
fn create_new_never_overwrites_existing() {
    let dir = std::env::temp_dir().join(format!("sdi-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("src.bin");
    let target = dir.join("target.jpg");
    std::fs::write(&src, b"NEWCONTENT").unwrap();
    std::fs::write(&target, b"ORIGINAL-IMPORTANT").unwrap();

    let mut buf = vec![0u8; 1024];
    let err = copy_one(&src, &target, &mut buf).unwrap_err();
    assert!(matches!(err, crate::error::AppError::TargetExists));
    assert_eq!(
        std::fs::read(&target).unwrap(),
        b"ORIGINAL-IMPORTANT",
        "原文件必须分毫不动"
    );

    // 目标不存在时可正常拷贝
    let target2 = dir.join("fresh.jpg");
    let n = copy_one(&src, &target2, &mut buf).unwrap();
    assert_eq!(n, 10);
    assert_eq!(std::fs::read(&target2).unwrap(), b"NEWCONTENT");

    std::thread::sleep(Duration::from_millis(10));
    let _ = std::fs::remove_dir_all(&dir);
}

/// sidecar 三形态关联（实测基准）
#[test]
fn sidecar_three_forms_all_resolve() {
    let mut dir: HashMap<String, DirFileMeta> = HashMap::new();
    let put = |m: &mut HashMap<String, DirFileMeta>, name: &str, kind: FileKind| {
        m.insert(
            name.to_lowercase(),
            DirFileMeta {
                file_name: name.to_string(),
                kind,
            },
        );
    };
    put(&mut dir, "P1161462.JPG", FileKind::Image);
    put(&mut dir, "P1160211.RW2", FileKind::Raw);
    put(&mut dir, "IMG_20260329_123640.dng", FileKind::Raw);

    let f1 = crate::meta::classify::find_main_file(&dir, "P1161462.xmp").unwrap();
    assert_eq!(f1.file_name, "P1161462.JPG");

    let f2 = crate::meta::classify::find_main_file(&dir, "P1160211.RW2.xmp").unwrap();
    assert_eq!(f2.file_name, "P1160211.RW2");

    let f3 = crate::meta::classify::find_main_file(&dir, "IMG_20260329_123640.dng.xmp").unwrap();
    assert_eq!(f3.file_name, "IMG_20260329_123640.dng");

    // 找不到主文件
    assert!(crate::meta::classify::find_main_file(&dir, "orphan.xmp").is_none());
}

/// 端到端（只读）：把库里已有的「某月」当虚拟卡扫描，
/// 验证完整链路：遍历 → 分类 → 时间解析 → 路径决策 → 三态判定。
/// 文件数动态统计（库会持续新增素材），不硬编码数量。
#[test]
fn scan_existing_month_all_exists() {
    let Some(root) = fixture_root() else {
        skip_msg();
        return;
    };
    let card = root.join(r"原图\2026\2026-10");
    if !card.is_dir() {
        skip_msg();
        return;
    }
    let cfg = crate::models::AppConfig {
        library_root: root.to_string_lossy().to_string(),
        ..Default::default()
    };
    let plan = crate::scanner::scan_card(&card.to_string_lossy(), &cfg).expect("扫描应成功");

    // 期望总数 = 目录实际文件数（排除系统文件）
    let expected = walkdir::WalkDir::new(&card)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().is_file()
                && !crate::meta::classify::is_ignored_name(&e.file_name().to_string_lossy())
        })
        .count();

    eprintln!(
        "扫描结果：总数 {}（目录 {}），新增 {}，已存在 {}，冲突 {}，改名跳过 {}",
        plan.total_files, expected, plan.new_files, plan.exists_files,
        plan.conflict_files, plan.suspect_files
    );

    assert_eq!(plan.total_files, expected, "扫描数应与目录实际文件数一致");
    assert_eq!(plan.new_files, 0, "自扫自库应全部判为已存在");
    assert_eq!(plan.exists_files, expected);
    assert_eq!(plan.conflict_files, 0, "不应有冲突（库未被改动）");
    assert_eq!(
        plan.suspect_files, 0,
        "自扫自库不应触发改名跳过（Exists 优先）"
    );
    assert!(plan.space_ok);
}

/// 改名场景端到端：临时库里放一个改名后的真实 JPG（内容相同），
/// 扫描 → 该文件应判为 SuspectSkipped（改名跳过），其余文件不受影响。
#[test]
fn renamed_file_in_library_is_skipped() {
    let Some(card) = fixture(r"原图\2026\2026-10") else {
        skip_msg();
        return;
    };
    let Some(fix_jpg) = fixture(r"原图\2026\2026-10\P1244659.JPG") else {
        skip_msg();
        return;
    };
    if !card.is_dir() || !fix_jpg.exists() {
        skip_msg();
        return;
    }

    // 临时库：放入改名副本 my_renamed_photo.jpg（内容 = 夹具 JPG）
    let lib = std::env::temp_dir().join(format!("sdi-lib-{}", std::process::id()));
    let lib_originals = lib.join("原图");
    std::fs::create_dir_all(&lib_originals).unwrap();
    std::fs::copy(&fix_jpg, lib_originals.join("my_renamed_photo.jpg")).unwrap();

    let cfg = crate::models::AppConfig {
        library_root: lib.to_string_lossy().to_string(),
        ..Default::default()
    };
    let plan = crate::scanner::scan_card(&card.to_string_lossy(), &cfg).expect("扫描应成功");

    let renamed = plan
        .files
        .iter()
        .find(|f| f.file_name.eq_ignore_ascii_case("P1244659.JPG"))
        .expect("计划中应包含夹具 JPG");
    assert_eq!(
        renamed.status,
        FileStatus::SuspectSkipped,
        "改名副本应通过签名判定被跳过，note = {:?}",
        renamed.note
    );
    assert!(renamed
        .note
        .as_deref()
        .unwrap_or("")
        .contains("my_renamed_photo.jpg"));

    // 其他文件不受影响：字节签名唯一，仍为新增
    let others_new = plan
        .files
        .iter()
        .filter(|f| !f.file_name.eq_ignore_ascii_case("P1244659.JPG"))
        .filter(|f| matches!(f.kind, FileKind::Image | FileKind::Raw))
        .all(|f| f.status == FileStatus::New);
    assert!(others_new, "其他照片不应被误判为改名跳过");
    assert_eq!(plan.suspect_files, 1);

    let _ = std::fs::remove_dir_all(&lib);
}

/// 三态判定（新语义）：判重对象是「快门产物是否已落库」，与库内后续编辑无关。
/// 库内编辑放大 / 裁剪缩小（尾部完整）→ 已存在；
/// 残缺半截（强杀残留特征：无 EOI）与非 JPEG 更小 → 冲突。
#[test]
fn stat_target_recognizes_edited_files() {
    let dir = std::env::temp_dir().join(format!("sdi-stat-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let big = synth_jpeg(&dir, "big.jpg", 128, 96);
    let small = synth_jpeg(&dir, "small.jpg", 32, 32);
    let big_len = std::fs::metadata(&big).unwrap().len();
    let small_len = std::fs::metadata(&small).unwrap().len();
    assert_ne!(big_len, small_len);

    use crate::models::FileStatus as FS;
    use crate::scanner::stat_target;

    // 等大 / 库内更大（评分 XMP、编辑放大）→ 已存在
    assert_eq!(stat_target(&big, big_len), FS::Exists);
    assert_eq!(stat_target(&big, small_len), FS::Exists);
    // 库内更小但 JPEG 尾部完整（裁剪 16:9 场景）→ 已存在
    assert_eq!(stat_target(&small, big_len), FS::Exists);

    // 残缺半截（截断，无 EOI）→ 冲突（强杀残留兜底）
    let truncated = dir.join("truncated.jpg");
    let half = &std::fs::read(&big).unwrap()[..(big_len as usize / 2)];
    std::fs::write(&truncated, half).unwrap();
    assert_eq!(stat_target(&truncated, big_len), FS::Conflict);

    // 非 JPEG 更小（视频/RAW 不会被原地编辑，更小只可能是残留）→ 冲突
    let raw = dir.join("P1234567.RW2");
    std::fs::write(&raw, b"small").unwrap();
    assert_eq!(stat_target(&raw, big_len), FS::Conflict);

    // 目标不存在 → 新增
    assert_eq!(stat_target(&dir.join("nope.jpg"), 100), FS::New);

    let _ = std::fs::remove_dir_all(&dir);
}

// ── 图库模块 ─────────────────────────────────────────────────────────────

/// 临时目录里造一张合成 JPEG（image crate 直写，无 EXIF）
fn synth_jpeg(dir: &std::path::Path, name: &str, w: u32, h: u32) -> PathBuf {
    let img = image::RgbImage::from_pixel(w, h, image::Rgb([180, 120, 40]));
    let path = dir.join(name);
    img.save_with_format(&path, image::ImageFormat::Jpeg).unwrap();
    path
}

/// G-01：Shell 缩略图生成（真实 COM 调用）+ 磁盘缓存命中
#[test]
fn shell_thumbnail_generates_and_caches() {
    let dir = std::env::temp_dir().join(format!("sdi-thumb-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let jpg = synth_jpeg(&dir, "sample.jpg", 640, 400);

    let meta = std::fs::metadata(&jpg).unwrap();
    let mtime = meta
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    // 首次：走 COM 生成
    let bytes = crate::thumbs::create(&jpg, mtime, meta.len()).expect("JPEG 应能生成 Shell 缩略图");
    assert!(bytes.len() > 500, "应产出实质 JPEG 内容，实际 {} 字节", bytes.len());
    assert_eq!(&bytes[..2], &[0xFF, 0xD8], "必须是 JPEG 魔数");

    // 缓存文件已落盘
    let cache_file = crate::thumbs::cache_dir().join(crate::thumbs::cache_key(&jpg, mtime, meta.len()));
    assert!(cache_file.exists(), "缓存文件应存在");

    // 二次：直接命中缓存（清掉源文件也能命中，证明没再走 COM）
    std::fs::remove_file(&jpg).unwrap();
    let cached = crate::thumbs::lookup(&jpg, mtime, meta.len()).expect("二次应命中缓存");
    assert_eq!(cached, bytes, "缓存内容应与首次生成一致");

    let _ = std::fs::remove_dir_all(&dir);
}

/// G-02：缓存键对 (路径, mtime, size) 敏感——文件变更自动失效
#[test]
fn thumb_cache_key_changes_with_metadata() {
    let dir = std::env::temp_dir().join(format!("sdi-thumbkey-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let jpg = synth_jpeg(&dir, "key.jpg", 64, 48);
    let k1 = crate::thumbs::cache_key(&jpg, 1000, 100);
    let k2 = crate::thumbs::cache_key(&jpg, 2000, 100);
    let k3 = crate::thumbs::cache_key(&jpg, 1000, 200);
    assert_ne!(k1, k2, "mtime 变化应换键");
    assert_ne!(k1, k3, "size 变化应换键");
    assert_eq!(k1, crate::thumbs::cache_key(&jpg, 1000, 100), "同输入同键");
    let _ = std::fs::remove_dir_all(&dir);
}

/// G-03：图库列表收集体积、类别过滤（合成临时库，不碰真实库）
#[test]
fn gallery_list_filters_kinds() {
    let lib = std::env::temp_dir().join(format!("sdi-glib-{}", std::process::id()));
    let months = lib.join("原图").join("2026").join("2026-01");
    std::fs::create_dir_all(&months).unwrap();
    let vdir = lib.join("视频").join("2026").join("2026-01");
    std::fs::create_dir_all(&vdir).unwrap();

    synth_jpeg(&months, "a.jpg", 32, 32);
    std::fs::write(months.join("b.RW2"), b"fake-raw").unwrap();
    std::fs::write(months.join("b.xmp"), b"sidecar").unwrap(); // 应排除
    std::fs::write(months.join("desktop.ini"), b"sys").unwrap(); // 应排除
    std::fs::write(months.join("note.txt"), b"unknown").unwrap(); // 应排除
    std::fs::write(vdir.join("c.mp4"), b"fake-video").unwrap(); // 应收录

    let cfg = crate::models::AppConfig {
        library_root: lib.to_string_lossy().to_string(),
        ..Default::default()
    };
    let items = crate::gallery::list_items(&cfg).expect("列表应成功");
    let mut names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
    names.sort();
    assert_eq!(names, vec!["a.jpg", "b.RW2", "c.mp4"], "只收 Image/Raw/Video");
    assert!(items.iter().all(|i| i.shoot_time.is_none()), "列表阶段不读时间");

    let _ = std::fs::remove_dir_all(&lib);
}

/// G-04（真实库夹具）：gallery::read_time 与已实测的 EXIF/mvhd 基准一致；
/// gallery::info 拿到相机制造商/型号；video_dimensions 拿到合理分辨率
#[test]
fn gallery_time_and_info_match_fixtures() {
    let Some(fix_rw2) = fixture(r"原图\2026\2026-10\P1244659.RW2") else {
        skip_msg();
        return;
    };
    let Some(fix_mp4) = fixture(r"视频\2026\2026-10\P1244742.MP4") else {
        skip_msg();
        return;
    };
    if !fix_rw2.exists() || !fix_mp4.exists() {
        skip_msg();
        return;
    }

    let t = crate::gallery::read_time(&fix_rw2, FileKind::Raw).expect("RW2 应解析出拍摄时间");
    assert_eq!(t, "2026-10-02 20:42:37");

    let t = crate::gallery::read_time(&fix_mp4, FileKind::Video).expect("MP4 应解析出拍摄时间");
    assert_eq!(t, "2026-10-07 14:57:08");

    let info = crate::gallery::info(&fix_rw2.to_string_lossy(), FileKind::Raw);
    assert_eq!(info.shoot_time.as_deref(), Some("2026-10-02 20:42:37"));
    let make = info.make.as_deref().unwrap_or("").to_lowercase();
    assert!(make.contains("panasonic"), "RW2 应含 Panasonic Make，实际 {:?}", info.make);
    assert!(info.model.is_some(), "RW2 应解析出机型");
    assert!(info.width.unwrap_or(0) >= 1000 && info.height.unwrap_or(0) >= 700, "RW2 传感器尺寸应合理，实际 {:?}", (info.width, info.height));

    let info = crate::gallery::info(&fix_mp4.to_string_lossy(), FileKind::Video);
    assert_eq!(info.shoot_time.as_deref(), Some("2026-10-07 14:57:08"));
    let (w, h) = (info.width.unwrap_or(0), info.height.unwrap_or(0));
    assert!(w >= 640 && h >= 360, "视频分辨率应合理，实际 {}x{}", w, h);
}

/// G-05（整库冒烟，`cargo test -- --ignored` 手动触发）：
/// 真实媒体库全量列表 + 时间读取抽样计时，只读。
#[test]
#[ignore]
fn gallery_full_library_smoke() {
    let Some(root) = fixture_root() else {
        skip_msg();
        return;
    };
    let cfg = crate::models::AppConfig {
        library_root: root.to_string_lossy().to_string(),
        ..Default::default()
    };

    let t0 = std::time::Instant::now();
    let items = crate::gallery::list_items(&cfg).expect("整库列表应成功");
    let list_secs = t0.elapsed().as_secs_f64();

    let by_kind = |k: FileKind| items.iter().filter(|i| i.kind == k).count();
    let total_bytes: u64 = items.iter().map(|i| i.size).sum();
    eprintln!(
        "整库列表 {:.2}s：{} 项（照片 {} / RAW {} / 视频 {}），共 {}",
        list_secs,
        items.len(),
        by_kind(FileKind::Image),
        by_kind(FileKind::Raw),
        by_kind(FileKind::Video),
        format_bytes(total_bytes),
    );

    // 时间读取抽样：前 30 个（含 RW2/JPG/MP4 混合），统计耗时与成功率
    let sample: Vec<_> = items.iter().take(30).map(|i| (i.path.clone(), i.kind)).collect();
    let t1 = std::time::Instant::now();
    let mut ok = 0;
    for (path, kind) in &sample {
        if crate::gallery::read_time(std::path::Path::new(path), *kind).is_some() {
            ok += 1;
        }
    }
    let per_file_ms = t1.elapsed().as_secs_f64() * 1000.0 / sample.len() as f64;
    eprintln!(
        "时间抽样 30 项：成功 {}，均耗 {:.1}ms/项 → 全库约 {:.0}s",
        ok,
        per_file_ms,
        per_file_ms * items.len() as f64 / 1000.0
    );
    assert!(ok >= 25, "抽样成功率过低：{ok}/30");
}

fn format_bytes(n: u64) -> String {
    if n >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", n as f64 / (1024 * 1024 * 1024) as f64)
    } else {
        format!("{:.1} MB", n as f64 / (1024 * 1024) as f64)
    }
}
