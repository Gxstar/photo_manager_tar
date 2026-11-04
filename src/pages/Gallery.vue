<script setup>
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { useVirtualizer } from "@tanstack/vue-virtual";
import { invoke } from "@tauri-apps/api/core";
// 注意：这个 WebView 里 window.confirm 返回的是 Promise（不是布尔），
// `!confirm(...)` 恒为 false，二次确认会静默失效 → 统一用原生对话框插件
import { ask } from "@tauri-apps/plugin-dialog";
import {
  Aperture,
  Film,
  FileImage,
  FolderOpen,
  Images,
  LayoutGrid,
  LoaderCircle,
  RefreshCw,
  Star,
} from "lucide-vue-next";
import { store, loadGallery, watchGalleryTimes, watchGalleryChanged, formatBytes, errMsg } from "../store.js";

// ── 几何常量 ─────────────────────────────────────────────────────────────
const CELL_MIN = 148; // 单元格最小宽（据此算列数）
const THUMB_H = 116; // 缩略图区高
const LABEL_H = 24; // 文件名行高
const ROW_H = THUMB_H + LABEL_H; // 行内容高
const GAP = 10; // 单元间距
const HEADER_H = 40; // 分组表头行高

// ── 布局 ─────────────────────────────────────────────────────────────────
const scrollEl = ref(null);
const contentEl = ref(null);
const cols = ref(4);
const cellW = ref(CELL_MIN);
let ro = null;

function measure() {
  const w = contentEl.value?.clientWidth ?? 0;
  if (w <= 0) return;
  cols.value = Math.max(2, Math.floor((w + GAP) / (CELL_MIN + GAP)));
  cellW.value = (w - (cols.value - 1) * GAP) / cols.value;
}

// ── 类型筛选 ─────────────────────────────────────────────────────────────
const kindFilter = ref("all"); // all | image | raw | video

const filterOpts = [
  { value: "all", label: "全部", icon: LayoutGrid },
  { value: "image", label: "照片", icon: FileImage },
  { value: "raw", label: "RAW", icon: Aperture },
  { value: "video", label: "视频", icon: Film },
];

// ── 评分筛选 ─────────────────────────────────────────────────────────────
// 只对「可评分文件」生效（JPEG + TIFF 容器 RAW）：选「未评」时不把视频等不可评分的项
// 混进来，否则几千个不可打分文件会把空档淹没。将来 RAW 支持 sidecar 评分，
// 改 isRatable 一处即可。
const ratingFilter = ref("all"); // all | picked | unrated

const ratingOpts = [
  { value: "all", label: "不限" },
  { value: "picked", label: "≥3★" },
  { value: "unrated", label: "未评" },
];

// 可评分格式：JPEG 与 TIFF 容器 RAW（XMP 都在文件内，后端原地改写）
// 与后端 xmp::RAW_EXTS 保持一致；视频与其他图片格式不参与评分
const RATABLE_EXTS = [
  "jpg", "jpeg",
  "rw2", "dng", "cr2", "nef", "arw", "orf", "raf", "pef",
];
const isRatable = (item) => RATABLE_EXTS.includes(item.ext);

const kindCounts = computed(() => {
  // 不含 all：总数已在工具条左侧「N 项」，「全部」按钮不重复展示
  const c = { image: 0, raw: 0, video: 0 };
  for (const it of store.gallery.items) if (it.kind in c) c[it.kind]++;
  return c;
});

const displayItems = computed(() => {
  const kind = kindFilter.value;
  const rating = ratingFilter.value;
  if (kind === "all" && rating === "all") return store.gallery.items;
  return store.gallery.items.filter((it) => {
    if (kind !== "all" && it.kind !== kind) return false;
    if (rating === "all") return true;
    if (!isRatable(it)) return false;
    const r = it.rating ?? 0;
    return rating === "picked" ? r >= 3 : r === 0;
  });
});

// 评分筛选出空结果时给出真实原因：三种空法（类型里没有可评分文件 /
// 还没人打高分 / 全都打过）用户看到的都是空网格，文案必须区分，
// 否则「≥3★ 为空」会被误读成「评分功能坏了」
const emptyHint = computed(() => {
  if (ratingFilter.value === "all") return null;
  const kind = kindFilter.value;
  const pool =
    kind === "all" ? store.gallery.items : store.gallery.items.filter((it) => it.kind === kind);
  if (!pool.some(isRatable)) return "评分筛选只对 JPG / RAW 生效，当前类型下没有可评分的文件";
  if (ratingFilter.value === "picked") return "还没有 ≥3★ 的照片，选中后按 0-5 即可打分";
  return "可评分的照片都已经打过分了";
});

// 切换筛选时清掉旧选择与键盘游标（跨筛选的选择集没有意义）
function resetSelection() {
  selected.value = new Set();
  anchorIdx.value = -1;
  cursorIdx.value = -1;
  hoverRating.value = 0;
}
watch([kindFilter, ratingFilter], resetSelection);

// 评分写入后目标可能立刻不满足筛选（例如在「≥3★」视图里降到 2★），
// 把已不在列表里的路径从选择集剔除，避免信息栏出现「选中 1 项」但看不到任何选中态
watch(displayItems, (list) => {
  if (!selected.value.size) return;
  const alive = new Set(list.map((it) => it.path));
  let dirty = false;
  for (const p of selected.value) {
    if (!alive.has(p)) {
      dirty = true;
      break;
    }
  }
  if (!dirty) return;
  const next = new Set();
  for (const p of selected.value) if (alive.has(p)) next.add(p);
  selected.value = next;
  if (!next.size) {
    anchorIdx.value = -1;
    cursorIdx.value = -1;
  }
});

// ── 行模型（月份分组表头 + 单元行）───────────────────────────────────────
// 拍摄时间补齐进行中不分组（补丁陆续到达会让表头乱跳），补齐完成后 /
// 时间缓存命中时才按「YYYY.MM」分组；无时间的归入「未标注时间」组。
// 每行带稳定 key：虚拟器尺寸缓存按键（非下标）存取，重排后按行身份重新测量
const rows = computed(() => {
  const groupable = store.gallery.timesTotal === 0;
  const out = [];
  let curKey = null;
  let cells = null;
  let header = null;
  for (let i = 0; i < displayItems.value.length; i++) {
    const it = displayItems.value[i];
    const key = groupable ? (it.shootTime ? it.shootTime.slice(0, 7) : "unknown") : "";
    if (key !== curKey) {
      curKey = key;
      header = null;
      if (groupable) {
        header = {
          type: "header",
          key: `h-${key}`,
          label: key === "unknown" ? "未标注时间" : key.replace("-", "."),
          count: 0,
        };
        out.push(header);
      }
      cells = null;
    }
    // 按列数分块：一个 cells 行最多 cols 个单元
    if (!cells || cells.items.length >= cols.value) {
      cells = { type: "cells", key: `c-${i}`, items: [], startIndex: i };
      out.push(cells);
    }
    if (header) header.count++;
    cells.items.push(it);
  }
  return out;
});

const rowVirtualizer = useVirtualizer(
  computed(() => ({
    count: rows.value.length,
    getScrollElement: () => scrollEl.value,
    estimateSize: (i) => (rows.value[i]?.type === "header" ? HEADER_H : ROW_H + GAP),
    getItemKey: (i) => rows.value[i]?.key ?? `i-${i}`,
    measureElement: (el) => el?.getBoundingClientRect().height ?? 0,
    overscan: 4,
  }))
);

// 动态测量：行元素挂载/尺寸变化时回报真实高度（与标称一致，自愈缓存）
function setRowRef(el) {
  if (el) rowVirtualizer.value?.measureElement(el);
}

// 行模型变化（筛选 / 重排 / 补齐完成）后让虚拟器按新行高重算
watch(rows, async () => {
  await nextTick();
  rowVirtualizer.value?.measure();
});

function rowAt(i) {
  return rows.value[i] ?? { type: "cells", items: [], startIndex: 0 };
}

// 行起始偏移前缀和（框选命中检测用）
const rowOffsets = computed(() => {
  const out = new Array(rows.value.length + 1);
  out[0] = 0;
  for (let i = 0; i < rows.value.length; i++) {
    out[i + 1] = out[i] + (rows.value[i].type === "header" ? HEADER_H : ROW_H + GAP);
  }
  return out;
});

function rowAtY(y) {
  const offs = rowOffsets.value;
  let lo = 0;
  let hi = rows.value.length - 1;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (offs[mid] <= y) lo = mid;
    else hi = mid - 1;
  }
  return lo;
}

// ── 加载 ─────────────────────────────────────────────────────────────────
const loadError = ref(null);
const hasLibrary = computed(() => !!store.config?.libraryRoot);

async function load() {
  loadError.value = null;
  try {
    await loadGallery();
  } catch (e) {
    loadError.value = errMsg(e);
  }
}

onMounted(() => {
  measure();
  ro = new ResizeObserver(measure);
  ro.observe(contentEl.value);
  watchGalleryTimes();
  watchGalleryChanged();
  window.addEventListener("keydown", onKeydown);
  load();
});
onBeforeUnmount(() => {
  ro?.disconnect();
  window.removeEventListener("keydown", onKeydown);
});

// ── 缩略图 ───────────────────────────────────────────────────────────────
const failed = ref(new Set());

function thumbUrl(path) {
  return `http://thumb.localhost/${encodeURIComponent(path)}`;
}

function onThumbError(path) {
  const next = new Set(failed.value);
  next.add(path);
  failed.value = next;
}

// ── 选择（对标资源管理器）────────────────────────────────────────────────
const selected = ref(new Set()); // Set<path>
const anchorIdx = ref(-1); // Shift 范围选择锚点
const cursorIdx = ref(-1); // 键盘游标（方向键从这里出发）
const band = ref(null); // 框选矩形（content 坐标 {x1,y1,x2,y2}）
let bandBase = null; // 框选起始时的选择集快照（Ctrl 叠加用）
let bandStart = null;
let suppressClick = false;

const singleItem = computed(() => {
  if (selected.value.size !== 1) return null;
  const path = [...selected.value][0];
  return displayItems.value.find((it) => it.path === path) ?? null;
});

function toggleSelect(path, additive) {
  const next = new Set(selected.value);
  if (next.has(path) && additive) next.delete(path);
  else next.add(path);
  selected.value = next;
}

function selectOnly(path) {
  selected.value = new Set([path]);
}

function onCellClick(item, idx, e) {
  if (suppressClick) return;
  cursorIdx.value = idx;
  if (e.ctrlKey) {
    toggleSelect(item.path, true);
    anchorIdx.value = idx;
  } else if (e.shiftKey && anchorIdx.value >= 0) {
    const [a, b] = [Math.min(anchorIdx.value, idx), Math.max(anchorIdx.value, idx)];
    const next = new Set();
    for (let i = a; i <= b; i++) {
      const it = displayItems.value[i];
      if (it) next.add(it.path);
    }
    selected.value = next;
  } else {
    selectOnly(item.path);
    anchorIdx.value = idx;
  }
}

// 框选：空白处 pointerdown 起步，移动 >4px 进入框选；
// 命中检测纯数学（行高列宽固定 → rect → 行列区间），不依赖虚拟 DOM
function contentPos(e) {
  const rect = contentEl.value.getBoundingClientRect();
  return {
    x: e.clientX - rect.left,
    y: e.clientY - rect.top + scrollEl.value.scrollTop,
  };
}

function onPointerDown(e) {
  if (e.button !== 0 || !scrollEl.value) return;
  if (e.target.closest("[data-cell]")) return;
  bandStart = contentPos(e);
  bandBase = e.ctrlKey || e.shiftKey ? new Set(selected.value) : new Set();
  if (!(e.ctrlKey || e.shiftKey)) selected.value = new Set();
  window.addEventListener("pointermove", onBandMove);
  window.addEventListener("pointerup", onBandUp, { once: true });
}

function onBandMove(e) {
  const p = contentPos(e);
  if (!band.value) {
    // 超过阈值才进入框选，避免误清单击
    if (Math.abs(p.x - bandStart.x) + Math.abs(p.y - bandStart.y) < 4) return;
    suppressClick = true;
  }
  band.value = {
    x1: Math.min(bandStart.x, p.x),
    y1: Math.min(bandStart.y, p.y),
    x2: Math.max(bandStart.x, p.x),
    y2: Math.max(bandStart.y, p.y),
  };
  // 命中检测：行偏移前缀和 → 行区间；行内按列宽求交，跳过表头行
  const w = contentEl.value.clientWidth;
  const cw = cellW.value + GAP;
  const r0 = rowAtY(band.value.y1);
  const r1 = rowAtY(band.value.y2);
  const c0 = Math.max(0, Math.floor(band.value.x1 / cw));
  const c1 = Math.floor(band.value.x2 / cw);
  const next = new Set(bandBase);
  for (let ri = r0; ri <= r1; ri++) {
    const r = rows.value[ri];
    if (r.type !== "cells") continue;
    const ry = rowOffsets.value[ri];
    for (let c = c0; c <= c1 && c < r.items.length; c++) {
      const cx = c * cw;
      // 单元矩形与框相交
      if (cx < band.value.x2 && cx + cellW.value > band.value.x1 && ry < band.value.y2 && ry + ROW_H > band.value.y1) {
        next.add(r.items[c].path);
      }
    }
  }
  selected.value = next;
}

function onBandUp() {
  window.removeEventListener("pointermove", onBandMove);
  const wasBand = !!band.value;
  band.value = null;
  bandStart = null;
  bandBase = null;
  if (wasBand) {
    // 拖框结束后吞掉紧跟的 click
    setTimeout(() => (suppressClick = false), 0);
  } else if (!suppressClick) {
    selected.value = new Set(); // 空白单击清除选择
  }
}

// ── 键盘操作（挑片时手不离键盘）─────────────────────────────────────────
// 映射：方向键移动（Shift 扩选）/ Home·End 首尾 / Enter 打开 /
// 0-5 打分（0 = 清除）/ Ctrl+A 全选 / Esc 取消选择。
// 事件挂在 window 上：单元格不可聚焦，靠 activeElement 判断会永远落空。

/** displayItems 下标 → 虚拟行下标（跳过表头行，键盘滚动定位用） */
const indexRow = computed(() => {
  const out = new Array(displayItems.value.length).fill(0);
  const rs = rows.value;
  for (let ri = 0; ri < rs.length; ri++) {
    const r = rs[ri];
    if (r.type !== "cells") continue;
    for (let k = 0; k < r.items.length; k++) out[r.startIndex + k] = ri;
  }
  return out;
});

/**
 * 把第 idx 个单元滚进视口。用 contentEl 的实时位置反算，不调虚拟器
 * scrollToIndex——那里的持久 scrollState 会干扰 measureElement 的尺寸自愈。
 */
function ensureVisible(idx) {
  const ri = indexRow.value[idx];
  if (ri == null || !contentEl.value || !scrollEl.value) return;
  const h = ROW_H + GAP;
  const cellTop = contentEl.value.getBoundingClientRect().top + rowOffsets.value[ri];
  const view = scrollEl.value.getBoundingClientRect();
  const pad = 8;
  if (cellTop < view.top + pad) {
    scrollEl.value.scrollTop -= view.top + pad - cellTop;
  } else if (cellTop + h > view.bottom) {
    scrollEl.value.scrollTop += cellTop + h - view.bottom + pad;
  }
}

function selectRange(a, b) {
  const lo = Math.min(a, b);
  const hi = Math.max(a, b);
  const next = new Set();
  for (let i = lo; i <= hi; i++) {
    const it = displayItems.value[i];
    if (it) next.add(it.path);
  }
  selected.value = next;
}

/** 相对移动（delta 为 ±1 或 ±cols）；extend = 按住 Shift 扩选 */
function moveBy(delta, extend) {
  const n = displayItems.value.length;
  if (!n) return;
  let from = cursorIdx.value;
  if (from < 0) from = delta >= 0 ? -1 : n;
  const next = Math.min(n - 1, Math.max(0, from + delta));
  cursorIdx.value = next;
  if (extend) selectRange(anchorIdx.value >= 0 ? anchorIdx.value : next, next);
  else {
    selected.value = new Set([displayItems.value[next].path]);
    anchorIdx.value = next;
  }
  ensureVisible(next);
}

/** 绝对移动（Home / End） */
function moveTo(idx, extend) {
  const n = displayItems.value.length;
  if (!n) return;
  const next = Math.min(n - 1, Math.max(0, idx));
  cursorIdx.value = next;
  if (extend) selectRange(anchorIdx.value >= 0 ? anchorIdx.value : next, next);
  else {
    selected.value = new Set([displayItems.value[next].path]);
    anchorIdx.value = next;
  }
  ensureVisible(next);
}

function onKeydown(e) {
  const el = e.target;
  const tag = (el?.tagName ?? "").toLowerCase();
  // 输入框/可编辑区域不抢键
  if (tag === "input" || tag === "textarea" || el?.isContentEditable) return;
  // 聚焦在按钮上时把 Enter / 空格留给按钮本身（刷新、筛选等）
  if (tag === "button" && (e.key === "Enter" || e.key === " ")) return;

  if (e.ctrlKey || e.metaKey) {
    if (e.key.toLowerCase() === "a") {
      e.preventDefault();
      const all = new Set(displayItems.value.map((it) => it.path));
      selected.value = all;
      anchorIdx.value = all.size ? 0 : -1;
      cursorIdx.value = displayItems.value.length - 1;
    }
    return;
  }
  if (e.altKey) return;

  const n = displayItems.value.length;
  if (!n) return;

  switch (e.key) {
    case "ArrowRight":
      e.preventDefault();
      moveBy(1, e.shiftKey);
      return;
    case "ArrowLeft":
      e.preventDefault();
      moveBy(-1, e.shiftKey);
      return;
    case "ArrowDown":
      e.preventDefault();
      moveBy(cols.value, e.shiftKey);
      return;
    case "ArrowUp":
      e.preventDefault();
      moveBy(-cols.value, e.shiftKey);
      return;
    case "Home":
      e.preventDefault();
      moveTo(0, e.shiftKey);
      return;
    case "End":
      e.preventDefault();
      moveTo(n - 1, e.shiftKey);
      return;
    case "Enter": {
      const it = selected.value.size === 1 ? singleItem.value : null;
      if (it) {
        e.preventDefault();
        openItem(it);
      }
      return;
    }
    case "Escape":
      selected.value = new Set();
      anchorIdx.value = -1;
      cursorIdx.value = -1;
      hoverRating.value = 0;
      return;
    default:
      // 0 = 清除评分（与点同一颗星等价）
      if (/^[0-5]$/.test(e.key)) {
        e.preventDefault();
        onRate(Number(e.key));
      }
  }
}

// ── 打开 ─────────────────────────────────────────────────────────────────
const openError = ref(null);

function openItem(item) {
  openError.value = null;
  invoke("gallery_open", { path: item.path }).catch((e) => {
    openError.value = errMsg(e);
  });
}

// ── 时间补齐完成时的重排 ─────────────────────────────────────────────────
// 不做滚动锚定：tanstack scrollToIndex 会留下持久 scrollState，
// 与动态测量（measureElement + shouldMeasureDuringScroll）相互作用会
// 压制后续行的尺寸修正，导致行错位。补齐多在数秒内完成、用户通常在页顶，
// 重排后自然停留在最新月份，无需锚定。

// ── 信息栏 ───────────────────────────────────────────────────────────────
const info = ref(null);
const infoLoading = ref(false);
let infoReq = 0;

watch(singleItem, async (item) => {
  info.value = null;
  if (!item) return;
  const req = ++infoReq;
  infoLoading.value = true;
  try {
    const data = await invoke("gallery_info", { path: item.path, kind: item.kind });
    if (req === infoReq) info.value = data;
  } catch {
    if (req === infoReq) info.value = null;
  } finally {
    if (req === infoReq) infoLoading.value = false;
  }
});

// ── 评分（XMP xmp:Rating，JPEG 与 RAW 均写文件内嵌 XMP）──────────────────
const hoverRating = ref(0);
const ratingBusy = ref(false);
const ratingError = ref(null);

const isJpeg = isRatable; // 语义别名：当前可评分 = JPG / RAW

// path → item（多选打分时避免 N×N 查找；Ctrl+A 全选后这是刚性需求）
const itemByPath = computed(() => {
  const m = new Map();
  for (const it of displayItems.value) m.set(it.path, it);
  return m;
});

// 当前可评分目标：单选 = 该文件；多选 = 选中项里的 JPEG
const rateTargets = computed(() => {
  if (!selected.value.size) return [];
  const map = itemByPath.value;
  const out = [];
  for (const p of selected.value) {
    const it = map.get(p);
    if (it && isRatable(it)) out.push(p);
  }
  return out;
});

// 信息栏当前显示的评分：info 现读值优先（能感知外部工具改动），回落到列表值
const curRating = computed(() => {
  const fromInfo = info.value?.rating;
  if (fromInfo != null) return fromInfo;
  return singleItem.value?.rating ?? 0;
});

async function onRate(i) {
  const targets = rateTargets.value;
  if (!targets.length || ratingBusy.value) return;
  // 单选：再点当前星 = 清除；多选：直接设为该值
  const value = targets.length === 1 && curRating.value === i ? null : i;
  // 大批量（Ctrl+A 后误操作）二次确认：每张都要整体重写文件，代价不低
  if (targets.length > 50) {
    const ok = await ask(`将给 ${targets.length} 张照片写入评分，继续？`, {
      title: "批量评分",
      kind: "warning",
      okLabel: "继续",
      cancelLabel: "取消",
    });
    if (!ok) return;
  }
  ratingBusy.value = true;
  ratingError.value = null;
  try {
    const failed = await invoke("gallery_set_rating", { paths: targets, rating: value });
    const v = value ?? 0;
    const set = new Set(targets);
    for (const it of store.gallery.items) {
      if (set.has(it.path)) it.rating = v > 0 ? v : null;
    }
    if (info.value && singleItem.value && set.has(singleItem.value.path)) {
      info.value.rating = value;
    }
    if (failed?.length) {
      ratingError.value = `${failed.length} 个文件写入失败：${failed[0]}${failed.length > 1 ? " 等" : ""}`;
    }
  } catch (e) {
    ratingError.value = errMsg(e);
  } finally {
    ratingBusy.value = false;
  }
}

// ── 展示辅助 ─────────────────────────────────────────────────────────────
const totalSize = computed(() => displayItems.value.reduce((s, it) => s + it.size, 0));
const timesProgress = computed(() => {
  const { timesDone, timesTotal } = store.gallery;
  if (timesTotal > 0 && timesDone < timesTotal) {
    return `${Math.min(timesDone, timesTotal)} / ${timesTotal}`;
  }
  return null;
});

function kindLabel(item) {
  if (item.kind === "video") return "视频";
  return item.kind === "raw" ? "RAW" : "照片";
}

function fmtDate(secs) {
  if (!secs) return "—";
  const d = new Date(secs * 1000);
  const p = (n) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

function cameraLabel(infoData) {
  const make = infoData?.make;
  const model = infoData?.model;
  if (make && model) {
    return model.toLowerCase().startsWith(make.toLowerCase()) ? model : `${make} ${model}`;
  }
  return model ?? make ?? null;
}

// ── 曝光参数格式化（后端给裸数值，展示约定放前端） ──────────────────────
const fmtNum = (v) => (Number.isInteger(v) ? String(v) : v.toFixed(1));

function fmtShutter(ex) {
  if (!ex?.num || !ex?.den) return null;
  const secs = ex.num / ex.den;
  if (secs >= 1) return `${Number(secs.toFixed(1))}s`;
  const inv = ex.den / ex.num;
  // 1/1600 这类倒数值应接近整数；4/10 这种写法则退回小数（0.4s）
  return Math.abs(inv - Math.round(inv)) < 0.02
    ? `1/${Math.round(inv)}`
    : `${secs.toFixed(1)}s`;
}

function fmtFocal(mm) {
  return `${fmtNum(Number(mm.toFixed(1)))}mm`;
}

const exposureRows = computed(() => {
  const d = info.value;
  if (!d) return [];
  const rows = [];
  if (d.fNumber) rows.push({ label: "光圈", value: `f/${fmtNum(Number(d.fNumber.toFixed(1)))}` });
  const shutter = fmtShutter(d.exposure);
  if (shutter) rows.push({ label: "快门", value: shutter });
  if (d.iso) rows.push({ label: "ISO", value: String(d.iso) });
  if (d.focalLength) {
    // 等效焦距单独一行小字：挤在同一行会把「43mm（86mm 等效）」折成两截
    const eq = d.focalLength35mm ? Math.round(d.focalLength35mm) : 0;
    rows.push({
      label: "焦距",
      value: fmtFocal(d.focalLength),
      sub: eq && eq !== Math.round(d.focalLength) ? `等效 ${eq}mm` : null,
    });
  }
  return rows;
});

function relDir(item) {
  const root = store.config?.libraryRoot ?? "";
  const p = item.path.replaceAll("/", "\\");
  const r = root.replaceAll("/", "\\");
  if (r && p.toLowerCase().startsWith(r.toLowerCase())) {
    return p.slice(r.length + 1).replace(/\\[^\\]*$/, "");
  }
  return "…";
}
</script>

<template>
  <div class="flex h-full min-h-0">
    <!-- 网格区 -->
    <div class="flex min-w-0 flex-1 flex-col">
      <!-- 工具条 -->
      <div class="flex h-11 shrink-0 items-center gap-3 overflow-hidden border-b border-line px-6 text-sm">
        <template v-if="store.gallery.items.length">
          <span class="shrink-0 whitespace-nowrap font-medium">{{ displayItems.length.toLocaleString() }} 项</span>
          <span v-if="kindFilter !== 'all' || ratingFilter !== 'all'" class="shrink-0 whitespace-nowrap text-xs text-ink-3">
            / 共 {{ store.gallery.items.length.toLocaleString() }} 项
          </span>
          <span class="shrink-0 text-ink-3">·</span>
          <span class="shrink-0 whitespace-nowrap text-ink-2">{{ formatBytes(totalSize) }}</span>
          <span v-if="timesProgress" class="flex min-w-0 items-center gap-1.5 truncate text-xs text-ink-2">
            <LoaderCircle class="size-3.5 shrink-0 animate-spin" />
            <span class="truncate">正在读取拍摄时间与评分 {{ timesProgress }}（首次较慢）</span>
          </span>
        </template>
        <span v-else class="shrink-0 whitespace-nowrap text-ink-2">图库</span>
        <span v-if="openError" class="shrink-0 whitespace-nowrap text-xs text-bad">{{ openError }}</span>
        <div v-if="store.gallery.items.length" class="ml-auto flex shrink-0 items-center gap-1.5">
          <div class="flex items-center gap-0.5 rounded-lg bg-subtle p-0.5">
            <button
              v-for="opt in filterOpts"
              :key="opt.value"
              class="flex h-6 shrink-0 items-center gap-1 whitespace-nowrap rounded-md px-2 text-xs transition-colors"
              :class="kindFilter === opt.value ? 'bg-surface text-ink shadow-sm' : 'text-ink-2 hover:text-ink'"
              @click="kindFilter = opt.value"
            >
              <component :is="opt.icon" class="size-3" />
              {{ opt.label }}
              <span v-if="opt.value !== 'all'" class="text-[10px] text-ink-3">{{ (kindCounts[opt.value] ?? 0).toLocaleString() }}</span>
            </button>
          </div>

          <span class="h-4 w-px shrink-0 bg-line"></span>

          <!-- 评分筛选：只筛可评分文件，避免不可打分的 RAW/视频淹没「未评」 -->
          <div
            class="flex items-center gap-0.5 rounded-lg bg-subtle p-0.5"
            title="按星级筛选（只对可评分的 JPG / RAW 生效）"
          >
            <Star class="mx-1 size-3 shrink-0 text-ink-3" />
            <button
              v-for="opt in ratingOpts"
              :key="opt.value"
              class="h-6 shrink-0 whitespace-nowrap rounded-md px-1.5 text-[11px] transition-colors"
              :class="ratingFilter === opt.value ? 'bg-surface font-medium text-ink shadow-sm' : 'text-ink-2 hover:text-ink'"
              @click="ratingFilter = opt.value"
            >
              {{ opt.label }}
            </button>
          </div>
        </div>
        <button
          class="flex h-7 shrink-0 items-center gap-1.5 whitespace-nowrap rounded-lg px-2.5 text-xs text-ink-2 transition-colors hover:bg-hover"
          :class="{ 'ml-auto': !store.gallery.items.length }"
          :disabled="store.gallery.loading"
          @click="load"
        >
          <RefreshCw class="size-3.5" :class="{ 'animate-spin': store.gallery.loading }" />
          刷新
        </button>
      </div>

      <!-- 错误 / 空态 -->
      <div v-if="loadError" class="mx-6 mt-4 rounded-xl bg-bad-soft px-4 py-3 text-sm text-bad">
        {{ loadError }}
      </div>
      <div v-else-if="!store.gallery.loading && !store.gallery.items.length" class="grid flex-1 place-items-center">
        <div class="text-center text-ink-3">
          <Images class="mx-auto mb-3 size-10" />
          <p class="text-sm">{{ hasLibrary ? "媒体库为空" : "请先在设置页配置媒体库路径" }}</p>
        </div>
      </div>
      <div v-else-if="!store.gallery.loading && !displayItems.length" class="grid flex-1 place-items-center">
        <div class="text-center">
          <p class="text-sm text-ink-3">当前筛选下没有文件</p>
          <p v-if="emptyHint" class="mt-1.5 text-xs text-ink-3">{{ emptyHint }}</p>
        </div>
      </div>

      <!-- 虚拟网格 -->
      <div
        ref="scrollEl"
        class="min-h-0 flex-1 overflow-y-auto px-6 pb-6 pt-4"
        @pointerdown="onPointerDown"
      >
        <div ref="contentEl" class="relative" :style="{ height: rowVirtualizer.getTotalSize() + 'px' }">
          <template v-for="vrow in rowVirtualizer.getVirtualItems()" :key="vrow.index">
            <!-- 月份分组表头 -->
            <div
              v-if="rowAt(vrow.index).type === 'header'"
              :ref="setRowRef"
              :data-index="vrow.index"
              class="absolute left-0 top-0 flex w-full items-center gap-2 pt-3"
              :style="{ transform: `translateY(${vrow.start}px)`, height: HEADER_H + 'px' }"
            >
              <span class="text-sm font-semibold">{{ rowAt(vrow.index).label }}</span>
              <span class="text-xs text-ink-3">{{ rowAt(vrow.index).count.toLocaleString() }} 项</span>
              <span class="ml-1 h-px flex-1 bg-line"></span>
            </div>

            <!-- 单元行 -->
            <div
              v-else
              :ref="setRowRef"
              :data-index="vrow.index"
              class="absolute left-0 top-0 w-full"
              :style="{
                transform: `translateY(${vrow.start}px)`,
                height: ROW_H + GAP + 'px',
                display: 'grid',
                gridTemplateColumns: `repeat(${cols}, minmax(0, 1fr))`,
                gap: GAP + 'px',
              }"
            >
              <div
                v-for="(item, localIdx) in rowAt(vrow.index).items"
                :key="item.path"
                data-cell
                class="cursor-default rounded-xl p-1 transition-colors"
                :class="selected.has(item.path) ? 'bg-amber-soft ring-1 ring-amber' : 'hover:bg-hover'"
                @click="onCellClick(item, rowAt(vrow.index).startIndex + localIdx, $event)"
                @dblclick="openItem(item)"
              >
                <div class="relative overflow-hidden rounded-lg bg-subtle" :style="{ height: THUMB_H + 'px' }">
                  <img
                    v-if="!failed.has(item.path)"
                    :src="thumbUrl(item.path)"
                    class="size-full object-cover"
                    loading="lazy"
                    decoding="async"
                    draggable="false"
                    @error="onThumbError(item.path)"
                  />
                  <div v-else class="grid size-full place-items-center text-ink-3">
                    <component :is="item.kind === 'video' ? Film : FileImage" class="size-8" />
                  </div>
                  <span
                    v-if="item.kind === 'video'"
                    class="absolute bottom-1 left-1 flex items-center gap-1 rounded bg-black/55 px-1.5 py-0.5 text-[10px] text-white"
                  >
                    <Film class="size-3" />
                    {{ item.ext.toUpperCase() }}
                  </span>
                  <!-- XMP 评分角标（有评分才显示） -->
                  <span
                    v-if="item.rating"
                    class="absolute bottom-1 left-1 flex items-center gap-0.5 rounded bg-black/55 px-1.5 py-0.5 text-[10px] font-medium text-amber-300"
                  >
                    <Star class="size-3 fill-current" />
                    {{ item.rating }}
                  </span>
                </div>
                <div class="truncate px-0.5 pt-1 text-xs leading-4 text-ink-2" :title="item.name">
                  {{ item.name }}
                </div>
              </div>
            </div>
          </template>

          <!-- 框选矩形（content 坐标系） -->
          <div
            v-if="band"
            class="pointer-events-none absolute z-10 rounded-md border border-amber bg-amber-soft/40"
            :style="{
              left: band.x1 + 'px',
              top: band.y1 + 'px',
              width: band.x2 - band.x1 + 'px',
              height: band.y2 - band.y1 + 'px',
            }"
          />
        </div>
      </div>
    </div>

    <!-- 右侧信息栏 -->
    <aside class="flex w-72 shrink-0 flex-col overflow-y-auto border-l border-line bg-surface">
      <template v-if="singleItem">
        <div class="border-b border-line px-5 pb-4 pt-5">
          <div class="mb-1.5 text-xs text-ink-3">{{ kindLabel(singleItem) }} · {{ singleItem.ext.toUpperCase() }}</div>
          <div class="break-all text-sm font-medium leading-5" :title="singleItem.name">{{ singleItem.name }}</div>
        </div>

        <div v-if="infoLoading" class="grid flex-1 place-items-center text-ink-3">
          <LoaderCircle class="size-5 animate-spin" />
        </div>
        <dl v-else class="flex flex-col gap-3 px-5 py-4 text-sm">
          <!-- 评分（JPEG 与 RAW 可写，RAW 写文件内嵌 XMP） -->
          <div v-if="isJpeg(singleItem)">
            <dt class="mb-1 text-xs text-ink-3">评分</dt>
            <dd class="flex items-center gap-0.5" @mouseleave="hoverRating = 0">
              <button
                v-for="i in 5"
                :key="i"
                class="rounded p-0.5 transition-transform"
                :class="{ 'scale-110': hoverRating === i, 'opacity-50': ratingBusy }"
                :disabled="ratingBusy"
                :title="i === curRating ? '清除评分' : `${i} 星`"
                @mouseenter="hoverRating = i"
                @click="onRate(i)"
              >
                <Star
                  class="size-[18px] transition-colors"
                  :class="(hoverRating || curRating) >= i ? 'fill-amber text-amber' : 'text-ink-3'"
                />
              </button>
              <span class="ml-1.5 text-xs text-ink-3">
                {{ hoverRating || curRating || "未评分" }}
              </span>
            </dd>
            <p v-if="ratingError" class="mt-1 text-xs text-bad">{{ ratingError }}</p>
          </div>
          <div>
            <dt class="mb-0.5 text-xs text-ink-3">拍摄时间</dt>
            <dd class="text-ink">{{ info?.shootTime ?? "—" }}</dd>
          </div>
          <div>
            <dt class="mb-0.5 text-xs text-ink-3">相机</dt>
            <dd class="text-ink">{{ cameraLabel(info) ?? "—" }}</dd>
          </div>
          <div v-if="info?.lens">
            <dt class="mb-0.5 text-xs text-ink-3">镜头</dt>
            <dd class="break-words text-ink">{{ info.lens }}</dd>
          </div>
          <!-- 曝光四件套：两列排布，缺项自动不占位（视频/无 EXIF 时不显示整块） -->
          <div v-if="exposureRows.length" class="grid grid-cols-2 gap-x-3 gap-y-2.5">
            <div v-for="row in exposureRows" :key="row.label">
              <dt class="mb-0.5 text-xs text-ink-3">{{ row.label }}</dt>
              <dd class="text-ink">
                {{ row.value }}
                <span v-if="row.sub" class="block text-[11px] leading-4 text-ink-3">{{ row.sub }}</span>
              </dd>
            </div>
          </div>
          <div>
            <dt class="mb-0.5 text-xs text-ink-3">分辨率</dt>
            <dd class="text-ink">{{ info?.width && info?.height ? `${info.width} × ${info.height}` : "—" }}</dd>
          </div>
          <div>
            <dt class="mb-0.5 text-xs text-ink-3">大小</dt>
            <dd class="text-ink">{{ formatBytes(singleItem.size) }}</dd>
          </div>
          <div>
            <dt class="mb-0.5 text-xs text-ink-3">修改时间</dt>
            <dd class="text-ink">{{ fmtDate(singleItem.mtimeSecs) }}</dd>
          </div>
          <div>
            <dt class="mb-0.5 text-xs text-ink-3">位置</dt>
            <dd class="break-all text-ink-2">{{ relDir(singleItem) }}</dd>
          </div>
        </dl>

        <div class="mt-auto border-t border-line p-4">
          <button
            class="flex h-8 w-full items-center justify-center gap-1.5 rounded-lg bg-subtle text-xs text-ink-2 transition-colors hover:bg-hover"
            @click="invoke('open_in_explorer', { path: singleItem.path })"
          >
            <FolderOpen class="size-3.5" />
            打开所在文件夹
          </button>
        </div>
      </template>

      <div v-else class="grid flex-1 place-items-center px-6">
        <div class="text-center">
          <p class="text-xs leading-5 text-ink-3">
            {{
              selected.size > 1
                ? `已选 ${selected.size} 项`
                : "选中一张照片或视频查看信息"
            }}
          </p>
          <!-- 键盘提示：挑片动作的发现成本（用户不知道有快捷键等于没有） -->
          <p v-if="selected.size <= 1 && displayItems.length" class="mt-3 text-[11px] leading-5 text-ink-3">
            方向键选择 · Shift 扩选 · Enter 打开<br />
            0-5 打分 · Ctrl+A 全选 · Esc 取消
          </p>
          <!-- 批量评分（选中项含 JPEG 时可用） -->
          <div v-if="selected.size > 1 && rateTargets.length" class="mt-4">
            <div class="mb-1.5 text-xs text-ink-3">
              批量评分（{{ rateTargets.length }} 张）
            </div>
            <!-- 不静默跳过：让用户知道哪些选中项不会被打分 -->
            <p v-if="selected.size > rateTargets.length" class="mb-1.5 text-[11px] text-ink-3">
              另有 {{ selected.size - rateTargets.length }} 个视频等文件不支持评分
            </p>
            <div
              class="flex items-center justify-center gap-0.5"
              @mouseleave="hoverRating = 0"
            >
              <button
                v-for="i in 5"
                :key="i"
                class="rounded p-0.5 transition-transform"
                :class="{ 'scale-110': hoverRating === i, 'opacity-50': ratingBusy }"
                :disabled="ratingBusy"
                :title="`${i} 星`"
                @mouseenter="hoverRating = i"
                @click="onRate(i)"
              >
                <Star
                  class="size-[18px] transition-colors"
                  :class="hoverRating >= i ? 'fill-amber text-amber' : 'text-ink-3'"
                />
              </button>
            </div>
            <p v-if="ratingError" class="mt-2 text-xs text-bad">{{ ratingError }}</p>
            <p v-else-if="ratingBusy" class="mt-2 text-xs text-ink-3">正在写入…</p>
          </div>
          <p v-else-if="selected.size > 1" class="mt-4 text-[11px] leading-5 text-ink-3">
            选中项没有可评分的文件<br />（只有 JPG 与 RAW 支持评分）
          </p>
        </div>
      </div>
    </aside>
  </div>
</template>
