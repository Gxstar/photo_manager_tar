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
