use chrono::NaiveDateTime;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

/// MP4/MOV 纪元偏移：mvhd 时间戳以 1904-01-01 为起点，Unix 以 1970-01-01。
/// 实测：P1244742.MP4 raw=3874201028 → 减去偏移 → 2026-10-07 14:57:08 (+08)，
/// 与其实际所在目录 视频\2026\2026-10 吻合。
const MP4_EPOCH_OFFSET: i64 = 2_082_844_800;

/// 视频拍摄时间：递归定位 moov>mvhd 的 creation_time。
/// 同一套 walker 覆盖 mp4 / mov / m4v（实测 MOV ftyp=qt 同样含 mvhd）。
pub fn video_shoot_time(path: &Path) -> Option<NaiveDateTime> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let (raw_secs, _) = find_mvhd(&mut reader, 0)?;
    let unix = raw_secs as i64 - MP4_EPOCH_OFFSET;
    chrono::DateTime::from_timestamp(unix, 0).map(|dt| dt.with_timezone(&chrono::Local).naive_local())
}

/// 顶层 box 遍历，进入容器 box（moov/trak/mdia/edts）递归找 mvhd
fn find_mvhd(
    reader: &mut BufReader<File>,
    depth: u32,
) -> Option<(u64, u8)> {
    if depth > 6 {
        return None;
    }
    loop {
        // box 头：size(4) type(4)；size==1 时后随 8 字节 largesize
        let mut hdr = [0u8; 8];
        if reader.read_exact(&mut hdr).is_err() {
            return None;
        }
        let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as u64;
        let typ = [hdr[4], hdr[5], hdr[6], hdr[7]];

        let (box_size, header_len) = if size32 == 1 {
            let mut ext = [0u8; 8];
            reader.read_exact(&mut ext).ok()?;
            (u64::from_be_bytes(ext), 16u64)
        } else if size32 == 0 {
            // size==0 表示 box 延伸到文件末尾
            let end = reader.seek(SeekFrom::End(0)).ok()?;
            reader.seek(SeekFrom::Start(end)).ok()?;
            return None;
        } else {
            (size32, 8u64)
        };

        if box_size < header_len {
            return None; // 损坏结构，防死循环
        }

        if &typ == b"mvhd" {
            // mvhd body: version(1) flags(3) creation_time(4 或 8) ...
            let mut body = [0u8; 12];
            reader.read_exact(&mut body).ok()?;
            let version = body[0];
            let ct = if version == 1 {
                let hi = u32::from_be_bytes(body[4..8].try_into().ok()?) as u64;
                let lo = u32::from_be_bytes(body[8..12].try_into().ok()?) as u64;
                (hi << 32) | lo
            } else {
                // version 0：creation_time 在 version+flags 之后的前 4 字节
                u32::from_be_bytes(body[4..8].try_into().ok()?) as u64
            };
            // version 0 的 creation_time 只读了 body[4..8]，version 1 需要更多字节——
            // 这里 body 只有 12 字节，version 1 时 hi 取 body[4..8]、lo 取 body[8..12]，正好 8 字节。OK。
            return Some((ct, version));
        }

        let body_start = reader.stream_position().ok()?;
        let is_container =
            matches!(&typ, b"moov" | b"trak" | b"mdia" | b"edts" | b"udta");

        if is_container {
            if let Some(found) = find_mvhd(reader, depth + 1) {
                return Some(found);
            }
            // 子层没找到，跳到本 box 结尾继续
            let after = body_start + (box_size - header_len);
            reader.seek(SeekFrom::Start(after)).ok()?;
        } else {
            // 跳过非容器 box
            reader
                .seek(SeekFrom::Start(body_start + (box_size - header_len)))
                .ok()?;
        }
    }
}
