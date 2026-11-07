# /// script
# requires-python = ">=3.10"
# dependencies = ["pillow>=10.0"]
# ///
"""阶段 0：媒体库 EXIF GPS 覆盖率只读统计（docs/开发方案.md §17.4.4 / §20）。

只读，不修改、不移动、不删除任何照片文件。
仅用于抽样核对 EXIF 解析结果；地点相册形态由应用内 Rust 扫描决定，
与覆盖率无关（docs §21：有多少 GPS 就展示多少）。
注意 Pillow 读不了 RW2 等 RAW，应用内的 Rust 解析才是实际数据来源。

用法：
    uv run tools/gps_coverage.py "D:\\媒体\\图片" --limit 500
    uv run tools/gps_coverage.py "D:\\媒体\\图片" --out gps_coverage.csv

注意：Pillow 无法解析的 RAW / HEIC 等格式计入「无法解析」，不参与 GPS 判定，
因此覆盖率是「可解析格式内的下界」，不是全库精确值 —— 结果请连同该限制一起看。
"""

from __future__ import annotations

import argparse
import csv
import os
import sys
from collections import Counter
from pathlib import Path

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8")

IMAGE_EXTS = {".jpg", ".jpeg", ".tif", ".tiff", ".png", ".webp", ".rw2", ".dng", ".heic", ".avif"}
GPS_IFD = 0x8825


def _to_deg(value) -> float | None:
    """GPS 度分秒 → 十进制度。"""
    try:
        d, m, s = float(value[0]), float(value[1]), float(value[2])
        return d + m / 60.0 + s / 3600.0
    except (TypeError, ValueError, IndexError, ZeroDivisionError):
        return None


def read_gps(path: Path):
    """返回 (lat, lon) 或 None；无法解析时抛异常由调用方计数。"""
    from PIL import Image

    with Image.open(path) as im:
        exif = im.getexif()
        if not exif:
            return None
        gps = exif.get_ifd(GPS_IFD)
        if not gps:
            return None
        lat = _to_deg(gps.get(2))
        lon = _to_deg(gps.get(4))
        if lat is None or lon is None:
            return None
        if str(gps.get(1, "N")).upper().startswith("S"):
            lat = -lat
        if str(gps.get(3, "E")).upper().startswith("W"):
            lon = -lon
        if not (-90 <= lat <= 90 and -180 <= lon <= 180):
            return None
        return round(lat, 6), round(lon, 6)


def collect(root: Path, limit: int):
    candidates: list[Path] = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in {"$RECYCLE.BIN", "PRIVATE", "System Volume Information"}]
        for name in filenames:
            if Path(name).suffix.lower() in IMAGE_EXTS:
                candidates.append(Path(dirpath) / name)
                if limit and len(candidates) >= limit:
                    return candidates
    return candidates


def main() -> int:
    ap = argparse.ArgumentParser(description="媒体库 EXIF GPS 覆盖率只读统计")
    ap.add_argument("root", help="媒体库根目录")
    ap.add_argument("--limit", type=int, default=0, help="只抽样前 N 个图片文件（0 = 全部）")
    ap.add_argument("--out", default="gps_coverage.csv", help="带 GPS 坐标的明细 CSV")
    args = ap.parse_args()

    root = Path(args.root)
    if not root.is_dir():
        print(f"目录不存在或不可访问：{root}")
        return 1

    files = collect(root, args.limit)
    if not files:
        print("未找到图片文件。")
        return 0

    with_gps: list[tuple[str, float, float]] = []
    unsupported = 0
    by_ext: Counter[str] = Counter()
    parsed_by_ext: Counter[str] = Counter()
    gps_by_ext: Counter[str] = Counter()

    for p in files:
        ext = p.suffix.lower()
        by_ext[ext] += 1
        try:
            pos = read_gps(p)
        except Exception:
            unsupported += 1
            continue
        parsed_by_ext[ext] += 1
        if pos:
            with_gps.append((str(p), pos[0], pos[1]))
            gps_by_ext[ext] += 1

    parsed = len(files) - unsupported
    print(f"扫描根目录：{root}")
    print(f"图片文件：{len(files)}")
    print(f"可解析：{parsed}　无法解析（RAW/HEIC 等）：{unsupported}")
    print(f"带 GPS 坐标：{len(with_gps)}")
    if parsed:
        print(f"覆盖率（相对可解析文件）：{len(with_gps) / parsed * 100:.1f}%")
    print(f"覆盖率（相对全部文件）：{len(with_gps) / len(files) * 100:.1f}%")
    print("按扩展名（带 GPS / 可解析）：")
    for ext, total in by_ext.most_common():
        print(f"  {ext:<7} {gps_by_ext[ext]:>6} / {parsed_by_ext[ext]:>6}")

    out = Path(args.out)
    with out.open("w", newline="", encoding="utf-8-sig") as f:
        w = csv.writer(f)
        w.writerow(["path", "lat", "lon"])
        w.writerows(with_gps)
    print(f"明细已写入：{out.resolve()}")
    print("提示：Pillow 无法解析的 RAW 不计入 GPS 判定；实际入库以应用内「扫描 GPS」为准。")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
