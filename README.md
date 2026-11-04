# 显影（develop）

插上相机 SD 卡 → 扫描 → 跳过库里已有的 → 按拍摄月份归档进 `原图\` / `视频\`。
个人本地工具，不联网、无数据库、不改文件名、绝不覆盖已有文件。

技术栈：Tauri 2 + Vue 3 + Vite + Tailwind CSS 4（前端）/ Rust（全部文件 IO）。

## 开发

```bash
npm install          # 首次
npm run tauri dev    # 开发模式（自动起 Vite + Rust 编译）
```

> 前提：安装 Rust 工具链（[rustup](https://rustup.rs)，MSVC target）；若命令行找不到 `cargo`，把 `%USERPROFILE%\.cargo\bin` 加入 PATH。
>
> 运行单元测试（可选）：测试用真实媒体库做只读夹具，设置环境变量 `SDI_FIXTURE_ROOT` 指向媒体库根目录，未设置时相关测试自动跳过。

## 打包

```bash
npm run tauri build  # 产出 NSIS 安装包（src-tauri/target/release/bundle/）
```

## 归档规则

- 目录结构：`{库根}\{原图|视频}\{YYYY}\{YYYY-MM}\原文件名`，年月目录自动创建
- 拍摄时间来源（降级链）：EXIF（RW2 走内嵌 APP1）→ MP4/MOV mvhd（1904 纪元换算）→ 文件名 `IMG_YYYYMMDD_HHMMSS` → 文件创建时间
- 无法确定时间 → `原图\待定\`；月份缺口是正常历史状态，不报错
- 去重 = 目标路径存在性检测（文件名 + 月），同名同大小跳过、不同大小标冲突，**绝不覆盖**
- sidecar（xmp/acr/pp3）跟随主文件同月归档
- 忽略 `desktop.ini` / `Thumbs.db` / `$RECYCLE.BIN` / `PRIVATE` 等系统目录

设计细节与实测依据见 `docs/开发方案.md`。
