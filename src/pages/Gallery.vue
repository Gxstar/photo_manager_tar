<script setup>
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { useVirtualizer } from "@tanstack/vue-virtual";
import { invoke } from "@tauri-apps/api/core";
// 注意：这个 WebView 里 window.confirm 返回的是 Promise（不是布尔），
// `!confirm(...)` 恒为 false，二次确认会静默失效 → 统一用原生对话框插件
import { ask } from "@tauri-apps/plugin-dialog";
import {
  Aperture,
  Check,
  ChevronDown,
  ExternalLink,
  Film,
  FileImage,
  FolderOpen,
  Heart,
  Images,
  LayoutGrid,
  LoaderCircle,
  PanelRight,
  PanelRightClose,
  RefreshCw,
  Star,
  WandSparkles,
} from "lucide-vue-next";
import { store, loadGallery, watchGalleryTimes, watchGalleryChanged, formatBytes, errMsg } from "../store.js";

// ── 缩略图尺寸档位（对标资源管理器「查看」菜单）──────────────────────────
// 用户选的是缩略图大小，列数由容器宽度自动算出。格宽与缩略图高按 ~1.28
// 等比联动 → 切档只是整体缩放，object-cover 的裁切比例不变。
// 最大档受后端 Shell 缩略图 256px 边长限制（thumbs.rs THUMB_SIZE）：164 CSS px
// 在 150% DPI 下约 246 物理像素仍够用，再大就会被拉伸变糊。
// 离散档位是刚性要求：行级虚拟化必须预先知道行高，自由滑块会让它每帧重测。
const DENSITIES = [
  { id: "small", label: "小", cell: 104, thumb: 78, icon: 12 },
  { id: "standard", label: "标准", cell: 148, thumb: 116, icon: 15 },
  { id: "large", label: "大", cell: 208, thumb: 164, icon: 18 },
];
const DENSITY_KEY = "develop.gallery.density";
const GAP = 10; // 单元间距
const LABEL_H = 24; // 文件名行高（不随档位变，字号固定 12px）
const HEADER_H = 40; // 分组表头行高

// ── 布局 ─────────────────────────────────────────────────────────────────
const scrollEl = ref(null);
const contentEl = ref(null);
const cols = ref(4);
const cellW = ref(DENSITIES[1].cell);
let ro = null;
// 工具条窄度：按工具条「实际布局宽度」降级。用 ResizeObserver 而非 CSS 断点：
// 阈值与侧栏 / 视口宽度解耦，也不受窗口缩放方式影响。
const toolbarEl = ref(null);
const toolbarNarrow = ref(false);
let toolbarRo = null;
const TOOLBAR_NARROW = 900; // 内容宽；1280 窗口约 992，1024 窗口约 736

function readDensity() {
  try {
    const id = localStorage.getItem(DENSITY_KEY);
    const i = DENSITIES.findIndex((d) => d.id === id);
    if (i >= 0) return i;
  } catch {
    // localStorage 被禁用时静默回落默认档
  }
  return 1; // 标准 = 原固定值 148 / 116
}
const densityIdx = ref(readDensity());
const density = computed(() => DENSITIES[densityIdx.value]);
const cellMin = computed(() => density.value.cell);
const thumbH = computed(() => density.value.thumb);
const rowH = computed(() => thumbH.value + LABEL_H); // 行内容高

function measure() {
  const w = contentEl.value?.clientWidth ?? 0;
  if (w <= 0) return;
  cols.value = Math.max(2, Math.round((w + GAP) / (cellMin.value + GAP)));
  cellW.value = (w - (cols.value - 1) * GAP) / cols.value;
  scheduleSticky(); // 宽度变了行可能重排，下一帧重算吸顶组
}

// ── 「查看」下拉菜单 ──────────────────────────────────────────────────────
// 菜单 Teleport 到 body + fixed 定位：不依赖祖先容器的 overflow，工具条换行后坐标仍按按钮 rect 算
const viewMenuOpen = ref(false);
const viewBtn = ref(null);
const menuPos = ref({ top: 0, left: 0 });
const MENU_W = 144;

function toggleViewMenu() {
  // 两个菜单互斥：打开「查看」就收起网格右键菜单
  ctxOpen.value = false;
  if (viewMenuOpen.value) {
    viewMenuOpen.value = false;
    return;
  }
  const r = viewBtn.value?.getBoundingClientRect();
  if (!r) return;
  menuPos.value = {
    top: r.bottom + 6,
    left: Math.max(8, Math.min(r.right - MENU_W, window.innerWidth - MENU_W - 8)),
  };
  viewMenuOpen.value = true;
}

function setDensity(i) {
  viewMenuOpen.value = false;
  if (i === densityIdx.value) return;
  // 切档会改列数与行高，先在旧布局上取锚点，重排后再把视口拉回原处
  const anchor = captureLayoutAnchor();
  densityIdx.value = i;
  try {
    localStorage.setItem(DENSITY_KEY, DENSITIES[i].id);
  } catch {
    // 存不了不影响本次会话的使用
  }
  scheduleRelayout(anchor);
}

function onGlobalPointerDown(e) {
  if (viewMenuOpen.value && !e.target.closest("[data-view-menu]")) viewMenuOpen.value = false;
  if (ctxOpen.value && !e.target.closest("[data-ctx-menu]")) ctxOpen.value = false;
}

// 窗口尺寸变了，fixed 菜单的坐标会失准，直接收起最简单
function onWindowResize() {
  viewMenuOpen.value = false;
  ctxOpen.value = false;
}

// ── 信息栏开合 ────────────────────────────────────────────────────────────
// 1280 宽时「侧栏 240 + 信息栏 288」占掉 528px，网格只剩一半；窄窗下把
// 信息栏收起是唯一能真正还照片以空间的做法（改列数治不了这个根）。
// 首次进入按窗口宽度决定，之后记住用户的选择。
const PANEL_KEY = "develop.gallery.panel";
function readPanelOpen() {
  try {
    const v = localStorage.getItem(PANEL_KEY);
    if (v === "0") return false;
    if (v === "1") return true;
  } catch {
    // 存不了就按窗口宽度给默认值
  }
  return window.innerWidth >= 1200;
}
const panelOpen = ref(readPanelOpen());

function togglePanel() {
  // 先记下旧布局的视口锚点：列数一变，同一个 scrollTop 会落到完全不同的内容上
  const anchor = captureLayoutAnchor();
  panelOpen.value = !panelOpen.value;
  try {
    localStorage.setItem(PANEL_KEY, panelOpen.value ? "1" : "0");
  } catch {
    // 记住与否不影响本次使用
  }
  scheduleRelayout(anchor);
}

// ── 类型筛选 ─────────────────────────────────────────────────────────────
const kindFilter = ref("all"); // all | image | raw | video

const filterOpts = [
  { value: "all", label: "全部", icon: LayoutGrid },
  { value: "image", label: "照片", icon: FileImage },
  { value: "raw", label: "RAW", icon: Aperture },
  { value: "video", label: "视频", icon: Film },
];

// ── 星级色阶 ─────────────────────────────────────────────────────────────
// 1★→5★：砖红 → 赭橙 → 琥珀金 → 橄榄 → 柔绿，五级等明度（色值在 tokens.css :root）。
// 每颗星按「组内位置」取色：评分 N 时点亮 1..N，最右那颗的颜色即代表评分等级。
// 角标星形用等级色、数字用白色；信息栏文字用中性墨色 —— 颜色只承载图形语义，
// 不承载小字号文字，保证 WCAG 文字对比度。未点亮星用 --star-off（白底 3.64:1）。
const STAR_COLORS = [1, 2, 3, 4, 5].map((n) => `var(--star-${n})`);
const starColor = (n) => STAR_COLORS[Math.max(1, Math.min(5, n)) - 1];
const starStyle = (n, lit) => ({ color: lit ? starColor(n) : "var(--star-off)" });

// ── 评分筛选 ─────────────────────────────────────────────────────────────
// 2026-11-05 起评分存 SQLite，与文件格式无关：RAW / 视频 / DNG 同样可评分。
// 取值：all 不限 / unrated 未评 / "1".."5" = ≥N★（含 N 星，与 Lightroom 口径一致）
const ratingFilter = ref("all");

const minStars = computed(() =>
  ratingFilter.value === "all" || ratingFilter.value === "unrated" ? 0 : Number(ratingFilter.value),
);

// 点第 N 颗星 = ≥N★；再点同一颗取消回「不限」
function setMinStars(n) {
  ratingFilter.value = ratingFilter.value === String(n) ? "all" : String(n);
}

// 悬停预览：鼠标划过第 N 颗时先亮到 N，离开恢复实际阈值
const hoverStars = ref(0);
const starPreview = computed(() => hoverStars.value || minStars.value);

// 评分存 SQLite，与格式无关：不再有「可评分白名单」，筛选 / 打分 / 批量
// 三条路径共用同一份 displayItems。

// ── 收藏筛选（二值 toggle，与类型 / 评分筛选 AND 叠加）──────────────────
const favoriteFilter = ref(false);
const favoriteCount = computed(
  () => store.gallery.items.filter((it) => it.favorite).length,
);

/** 一键回到「全部」：筛选组合出空结果时，用户需要一条明确的退路 */
function clearFilters() {
  kindFilter.value = "all";
  ratingFilter.value = "all";
  favoriteFilter.value = false;
}

const kindCounts = computed(() => {
  // 不含 all：总数已在工具条左侧「N 项」，「全部」按钮不重复展示
  const c = { image: 0, raw: 0, video: 0 };
  for (const it of store.gallery.items) if (it.kind in c) c[it.kind]++;
  return c;
});

const displayItems = computed(() => {
  const kind = kindFilter.value;
  const rating = ratingFilter.value;
  const fav = favoriteFilter.value;
  if (kind === "all" && rating === "all" && !fav) return store.gallery.items;
  return store.gallery.items.filter((it) => {
    if (fav && !it.favorite) return false;
    if (kind !== "all" && it.kind !== kind) return false;
    if (rating === "all") return true;
    const r = it.rating ?? 0;
    if (rating === "unrated") return r === 0;
    return r >= Number(rating);
  });
});

// 筛选为空时给出真实原因，否则「≥3★ 为空」会被误读成「功能坏了」
const emptyHint = computed(() => {
  if (favoriteFilter.value) {
    return favoriteCount.value
      ? "当前筛选下没有已收藏的文件"
      : "还没有收藏任何文件，选中后按空格即可收藏";
  }
  if (ratingFilter.value === "all") return null;
  const kind = kindFilter.value;
  const pool =
    kind === "all" ? store.gallery.items : store.gallery.items.filter((it) => it.kind === kind);
  if (!pool.length) return "当前类型下没有文件";
  if (ratingFilter.value === "unrated") return "当前视图里的文件都已经打过分了";
  return `还没有 ≥${minStars.value}★ 的照片，选中后按 0-5 即可打分`;
});

// 切换筛选时清掉旧选择与键盘游标（跨筛选的选择集没有意义）
function resetSelection() {
  selected.value = new Set();
  anchorIdx.value = -1;
  cursorIdx.value = -1;
  hoverRating.value = 0;
  hoverStars.value = 0;
}
watch([kindFilter, ratingFilter, favoriteFilter], resetSelection);

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
    estimateSize: (i) => (rows.value[i]?.type === "header" ? HEADER_H : rowH.value + GAP),
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
  scheduleSticky();
});

// 切档的重排与视口锚定统一走 setDensity → scheduleRelayout：
// 容器宽度没变，ResizeObserver 不会触发，必须主动重算 cols 与清虚拟器尺寸缓存
// （即使 cols 恰好不变也要清，否则残留旧行高会错位）。

function rowAt(i) {
  return rows.value[i] ?? { type: "cells", items: [], startIndex: 0 };
}

// 行起始偏移前缀和（框选命中检测用）
const rowOffsets = computed(() => {
  const out = new Array(rows.value.length + 1);
  out[0] = 0;
  for (let i = 0; i < rows.value.length; i++) {
    out[i + 1] = out[i] + (rows.value[i].type === "header" ? HEADER_H : rowH.value + GAP);
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

// ── 分组表头吸顶 ──────────────────────────────────────────────────────────
// 虚拟器只保留可视区 ±4 行，月份表头滚过即被卸载；且表头是
// absolute + translateY 的行，直接给行加 CSS sticky 无效。所以走「旁路浮层」：
//   · 浮层挂在「不滚动」的包装层上（absolute top-0），与滚动容器平级 →
//     contentEl 位置、虚拟器坐标、框选坐标系一律不变，且紧贴工具条
//   · 不用 sticky：滚动容器有 pt-4，sticky 贴的是内容区起点，那条 16px
//     会漏出上一行照片；sticky 粘附范围还受 containing block 限制
//   · 当前月份由 scrollTop 在 headerRows 上二分得出；下一组表头距顶
//     小于 HEADER_H 时把浮层向上推走，形成「被下一个月份替换」的交接
const headerRows = computed(() => {
  const offs = rowOffsets.value;
  const out = [];
  for (let i = 0; i < rows.value.length; i++) {
    const r = rows.value[i];
    if (r.type === "header") out.push({ top: offs[i], label: r.label, count: r.count });
  }
  return out;
});

const stickyHeader = ref(null); // { label, count } | null
const stickyShift = ref(0); // 负值 = 正被下一组表头顶出视口
let stickyRaf = 0;

function updateSticky() {
  stickyRaf = 0;
  const el = scrollEl.value;
  const hs = headerRows.value;
  if (!el || !contentEl.value || !hs.length) {
    stickyHeader.value = null;
    stickyShift.value = 0;
    return;
  }
  // 表头 i 距滚动区可视顶 = (contentEl.top - scrollEl.top) + rowOffsets[i]
  const base = contentEl.value.getBoundingClientRect().top - el.getBoundingClientRect().top;
  let lo = 0;
  let hi = hs.length - 1;
  let cur = -1; // 最后一个已滚过顶的表头
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (hs[mid].top + base <= 0) {
      cur = mid;
      lo = mid + 1;
    } else hi = mid - 1;
  }
  if (cur < 0) {
    // 首组表头还在原位，交给真实表头行显示，避免同一月份出现两份
    stickyHeader.value = null;
    stickyShift.value = 0;
    return;
  }
  const h = hs[cur];
  // 只有换月份才更新文本，滚动过程中不制造无谓渲染
  if (stickyHeader.value?.label !== h.label || stickyHeader.value?.count !== h.count) {
    stickyHeader.value = { label: h.label, count: h.count };
  }
  const nextTop = cur + 1 < hs.length ? hs[cur + 1].top + base : Infinity;
  stickyShift.value = nextTop >= HEADER_H ? 0 : Math.max(-HEADER_H, nextTop - HEADER_H);
}

// 滚动高频触发，合并到一帧；行模型 / 档位 / 容器宽度变化也复用同一入口
function scheduleSticky() {
  if (!stickyRaf) stickyRaf = requestAnimationFrame(updateSticky);
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
  // 信息栏开合 / 窗口缩放都会改 contentEl 宽度；在旧布局上取锚点后统一重排
  ro = new ResizeObserver(() => scheduleRelayout(captureLayoutAnchor()));
  ro.observe(contentEl.value);
  toolbarRo = new ResizeObserver(() => {
    // clientWidth 含 px-6 左右 padding（共 48），换算成内容宽再比阈值
    toolbarNarrow.value = (toolbarEl.value?.clientWidth ?? 0) < TOOLBAR_NARROW + 48;
  });
  toolbarRo.observe(toolbarEl.value);
  watchGalleryTimes();
  watchGalleryChanged();
  window.addEventListener("keydown", onKeydown);
  window.addEventListener("pointerdown", onGlobalPointerDown);
  window.addEventListener("resize", onWindowResize);
  load();
});
onBeforeUnmount(() => {
  ro?.disconnect();
  toolbarRo?.disconnect();
  if (stickyRaf) cancelAnimationFrame(stickyRaf);
  window.removeEventListener("keydown", onKeydown);
  window.removeEventListener("pointerdown", onGlobalPointerDown);
  window.removeEventListener("resize", onWindowResize);
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
      if (cx < band.value.x2 && cx + cellW.value > band.value.x1 && ry < band.value.y2 && ry + rowH.value > band.value.y1) {
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
  const h = rowH.value + GAP;
  const cellTop = contentEl.value.getBoundingClientRect().top + rowOffsets.value[ri];
  const view = scrollEl.value.getBoundingClientRect();
  const pad = 8;
  // 分组表头吸顶会遮住顶部 40px：键盘滚到第一行时留出这条高度，
  // 否则 ↑ / Home 会「选中一个看不见的格子」
  const padTop = headerRows.value.length ? HEADER_H + pad : pad;
  if (cellTop < view.top + padTop) {
    scrollEl.value.scrollTop -= view.top + padTop - cellTop;
  } else if (cellTop + h > view.bottom) {
    scrollEl.value.scrollTop += cellTop + h - view.bottom + pad;
  }
}

// ── 列数变化时的视觉锚定 ──────────────────────────────────────────────────
// rows 按 cols 分块，列数一变，同一个 scrollTop 会落到完全不同的内容上
// （信息栏开合 / 窗口缩放 / 缩略图切档都会触发）。重排前记下锚点单元相对视口
// 的偏移，重排后用新的 rowOffsets 把 scrollTop 调回去，锚点留在原来的屏幕位置。
// 直接改 scrollTop，不调虚拟器 scrollToIndex（持久 scrollState 会压制
// measureElement 的尺寸自愈，见 ensureVisible 上方注释）。
let pendingAnchor; // undefined = 无待处理；null = 无有效锚点
let relayoutScheduled = false;

/** 视口顶部第一个单元在 displayItems 里的下标 */
function topVisibleIndex() {
  const el = scrollEl.value;
  const content = contentEl.value;
  if (!el || !content) return -1;
  const base = content.getBoundingClientRect().top - el.getBoundingClientRect().top;
  const ri = rowAtY(-base);
  for (let r = ri; r < rows.value.length; r++) {
    const row = rows.value[r];
    if (row.type === "cells" && row.items.length) return row.startIndex;
  }
  return -1;
}

/** 锚点优先单选中的照片（用户正要看它）；否则退到视口顶部第一个单元 */
function captureLayoutAnchor() {
  const el = scrollEl.value;
  const content = contentEl.value;
  if (!el || !content) return null;
  let idx = -1;
  const sel = singleItem.value;
  if (sel) idx = displayItems.value.findIndex((it) => it.path === sel.path);
  if (idx < 0) idx = topVisibleIndex();
  if (idx < 0) return null;
  const ri = indexRow.value[idx];
  if (ri == null) return null;
  const cellTop = content.getBoundingClientRect().top + rowOffsets.value[ri];
  return {
    path: displayItems.value[idx]?.path ?? null,
    idx,
    offset: cellTop - el.getBoundingClientRect().top,
  };
}

function restoreLayoutAnchor(anchor) {
  const el = scrollEl.value;
  const content = contentEl.value;
  if (!anchor || !el || !content) return;
  let idx = anchor.path
    ? displayItems.value.findIndex((it) => it.path === anchor.path)
    : anchor.idx;
  if (idx < 0 || idx >= displayItems.value.length) return;
  const ri = indexRow.value[idx];
  if (ri == null) return;
  // 分组表头吸顶会盖住顶部 40px：锚点至少留在表头下方
  const minOffset = headerRows.value.length ? HEADER_H + 8 : 8;
  const target = Math.max(anchor.offset, minOffset);
  const cellTop = content.getBoundingClientRect().top + rowOffsets.value[ri];
  el.scrollTop += cellTop - (el.getBoundingClientRect().top + target);
  ensureVisible(idx); // 兜底：重排后行高估算有误差时仍滚回可见区
  scheduleSticky();
}

/** 合并同一帧里的多次重排（拖窗口时 ResizeObserver 会连续触发） */
function scheduleRelayout(anchor) {
  if (anchor !== undefined && pendingAnchor === undefined) pendingAnchor = anchor;
  if (relayoutScheduled) return;
  relayoutScheduled = true;
  nextTick(async () => {
    relayoutScheduled = false;
    const a = pendingAnchor ?? null;
    pendingAnchor = undefined;
    const before = cols.value;
    measure();
    await nextTick();
    rowVirtualizer.value?.measure();
    // 列数没变就不动 scrollTop，避免无谓抖动
    if (a && cols.value !== before) {
      await nextTick();
      restoreLayoutAnchor(a);
    }
    scheduleSticky();
  });
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
  // 空格无条件归收藏（用户拍板）；按钮仍可用 Enter 激活。
  // 焦点在按钮上时也不让位，空格分支会 preventDefault 阻止按钮被重复激活。
  if (tag === "button" && e.key === "Enter") return;
  // 任一菜单打开时网格快捷键先让位（Esc 除外，下面用它收菜单）
  if ((viewMenuOpen.value || ctxOpen.value) && e.key !== "Escape") return;

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

  // 空格独占收藏切换：无条件 preventDefault（否则会滚动页面 / 激活焦点按钮）
  if (e.key === " " || e.code === "Space") {
    e.preventDefault();
    onToggleFavorite();
    return;
  }

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
      // 先收菜单，不连带清掉选择（对标资源管理器：Esc 只关当前浮层）
      if (ctxOpen.value) {
        ctxOpen.value = false;
        return;
      }
      if (viewMenuOpen.value) {
        viewMenuOpen.value = false;
        return;
      }
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

// ── 网格右键菜单（只负责「把文件交给哪个程序」，不改文件）─────────────────
// 与「查看」菜单同套路：Teleport 到 body + fixed，避免被 overflow-y-auto / 虚拟行 transform 裁切。
const CTX_W = 200;
const CTX_H = 144; // 头部 + 分隔线 + 3 项 + 分隔线 + padding，用于右下越界夹取
const MAX_CTX_OPEN = 20; // Ctrl+A 全选后误点「打开」会拉起上万进程，必须兜住
const ctxOpen = ref(false);
const ctxPos = ref({ top: 0, left: 0 });
const ctxItems = ref([]); // 打开菜单时的目标快照，避免选择变化导致动作打偏
const ctxTooMany = computed(() => ctxItems.value.length > MAX_CTX_OPEN);
const ctxTitle = computed(() =>
  ctxItems.value.length > 1
    ? `已选 ${ctxItems.value.length} 项`
    : (ctxItems.value[0]?.name ?? ""),
);

function onCellContextMenu(item, idx, e) {
  e.preventDefault(); // 屏蔽 WebView2 原生右键菜单
  viewMenuOpen.value = false;
  // 对标资源管理器：右键未选中项只选它；落在已选中项上保留整组
  if (!selected.value.has(item.path)) {
    selectOnly(item.path);
    anchorIdx.value = idx;
    cursorIdx.value = idx;
  }
  const set = selected.value;
  ctxItems.value = displayItems.value.filter((it) => set.has(it.path));
  if (!ctxItems.value.length) ctxItems.value = [item];
  ctxPos.value = {
    left: Math.max(8, Math.min(e.clientX, window.innerWidth - CTX_W - 8)),
    top: Math.max(8, Math.min(e.clientY, window.innerHeight - CTX_H - 8)),
  };
  ctxOpen.value = true;
}

// 网格空白区右键：只关已有菜单 + 屏蔽原生菜单，不改选择
function onAreaContextMenu(e) {
  if (!e.target.closest("[data-cell]")) ctxOpen.value = false;
}

// 网格滚动后 fixed 菜单的坐标就失效了，直接收起
function onGridScroll() {
  if (ctxOpen.value) ctxOpen.value = false;
  scheduleSticky();
}

async function ctxOpenDefault() {
  const items = ctxItems.value.slice();
  ctxOpen.value = false;
  if (!items.length || items.length > MAX_CTX_OPEN) return;
  openError.value = null;
  for (const it of items) {
    try {
      await invoke("gallery_open", { path: it.path });
    } catch (e) {
      openError.value = errMsg(e);
      return;
    }
  }
}

async function ctxOpenPhotoshop() {
  const paths = ctxItems.value.map((it) => it.path);
  ctxOpen.value = false;
  if (!paths.length || paths.length > MAX_CTX_OPEN) return;
  openError.value = null;
  try {
    await invoke("open_with_photoshop", { paths });
  } catch (e) {
    openError.value = errMsg(e);
  }
}

function ctxOpenFolder() {
  const item = ctxItems.value[0];
  ctxOpen.value = false;
  if (!item) return;
  openError.value = null;
  invoke("open_in_explorer", { path: item.path }).catch((e) => {
    openError.value = errMsg(e);
  });
}

// ── 时间补齐完成时的重排 ─────────────────────────────────────────────────
// 不做滚动锚定：tanstack scrollToIndex 会留下持久 scrollState，
// 与动态测量（measureElement + shouldMeasureDuringScroll）相互作用会
// 压制后续行的尺寸修正，导致行错位。补齐多在数秒内完成、用户通常在页顶，
// 重排后自然停留在最新月份，无需锚定。

// ── 信息栏 ───────────────────────────────────────────────────────────────
// 评分错误提示要在「切换选中」时清空，故声明提前到信息栏 watch 之前
const ratingError = ref(null);
const favError = ref(null);
const info = ref(null);
const infoLoading = ref(false);
let infoReq = 0;

watch(singleItem, async (item) => {
  info.value = null;
  // 切换选中时清掉上一次的写入失败提示，否则会挂在新文件信息栏里张冠李戴
  ratingError.value = null;
  favError.value = null;
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

// ── 评分（只写 SQLite ratings 表，绝不改图片文件）─────────────────────────
const hoverRating = ref(0);
const ratingBusy = ref(false);

// path → item（多选打分时避免 N×N 查找；Ctrl+A 全选后这是刚性需求）
const itemByPath = computed(() => {
  const m = new Map();
  for (const it of displayItems.value) m.set(it.path, it);
  return m;
});

// 当前打分目标：全部选中的媒体文件（评分与文件格式无关）
const rateTargets = computed(() => {
  if (!selected.value.size) return [];
  const map = itemByPath.value;
  const out = [];
  for (const p of selected.value) {
    if (map.has(p)) out.push(p);
  }
  return out;
});

// 信息栏当前显示的评分（列表 DTO 即权威值）
const curRating = computed(() => singleItem.value?.rating ?? 0);

async function onRate(i) {
  const targets = rateTargets.value;
  if (!targets.length || ratingBusy.value) return;
  // 单选：再点当前星 = 清除；多选：直接设为该值
  const value = targets.length === 1 && curRating.value === i ? null : i;
  // 大批量（Ctrl+A 后误操作）二次确认
  if (targets.length > 50) {
    const ok = await ask(`将给 ${targets.length} 个文件写入评分（不修改照片文件），继续？`, {
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
    await invoke("gallery_set_rating", { paths: targets, rating: value });
    const v = value ?? 0;
    const set = new Set(targets);
    for (const it of store.gallery.items) {
      if (set.has(it.path)) it.rating = v > 0 ? v : null;
    }
  } catch (e) {
    // DB 写失败后端会明确报错，这里如实显示，绝不假装已保存
    ratingError.value = errMsg(e);
  } finally {
    ratingBusy.value = false;
  }
}

// ── 收藏（只写 SQLite favorites 表，绝不改图片文件）──────────────────────
const favBusy = ref(false);
const allSelectedFav = computed(
  () =>
    rateTargets.value.length > 0 &&
    rateTargets.value.every((p) => itemByPath.value.get(p)?.favorite),
);

// 单选看这一张，多选看选区是否全已收藏 —— 同一个按钮两种语义
const favActive = computed(() =>
  singleItem.value ? !!singleItem.value.favorite : allSelectedFav.value,
);

function toggleFavTargets() {
  onToggleFavorite(singleItem.value ? [singleItem.value.path] : null);
}

// 单选 toggle；多选：选区全部已收藏 → 全部取消，否则 → 全部收藏。
// 不逐个 toggle，避免混合选区敲一次空格后状态更乱。
async function onToggleFavorite(overridePaths = null) {
  const targets = overridePaths ?? rateTargets.value;
  if (!targets.length || favBusy.value) return;
  const allFav = targets.every((p) => itemByPath.value.get(p)?.favorite);
  const value = !allFav;
  favBusy.value = true;
  favError.value = null;
  try {
    await invoke("gallery_set_favorite", { paths: targets, favorite: value });
    const set = new Set(targets);
    for (const it of store.gallery.items) {
      if (set.has(it.path)) it.favorite = value;
    }
  } catch (e) {
    // DB 写失败后端会明确报错，如实显示，不假装已保存
    favError.value = errMsg(e);
  } finally {
    favBusy.value = false;
  }
}

// 缩略图红心：单击切换这一张（不依赖当前选区）
function onHeartClick(item, e) {
  e.stopPropagation();
  onToggleFavorite([item.path]);
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
  <div
    class="grid h-full min-h-0 grid-rows-[auto_minmax(0,1fr)]"
    :class="panelOpen ? 'grid-cols-[minmax(0,1fr)_18rem]' : 'grid-cols-[minmax(0,1fr)]'"
  >
    <!-- 全宽工具条：横跨网格区 + 信息栏，不再受网格区宽度限制。
         固定单行：筛选后左侧统计会多出「/ 共 N 项」「≥N★」，若允许换行，
         刚性的筛选组就会被整组挤到第二行。左侧统计是唯一可收缩区（空间不足
         时从右往左裁低优先级文字），右侧控件刚性保留；极窄时低价值信息先让位，
         仍放不下才横向滚动。 -->
    <!-- min-w-0：作为 grid item 不按内容撑宽，窄窗口才会走工具条内部横向滚动 -->
    <div ref="toolbarEl" class="toolbar col-span-full flex min-h-11 min-w-0 flex-nowrap items-center gap-x-3 overflow-x-auto border-b border-line px-6 py-1.5 text-sm">
      <!-- 左侧统计：唯一弹性区，min-w-0 才能真的收缩 -->
      <div class="flex min-w-0 flex-1 items-center gap-x-3 overflow-hidden">
        <template v-if="store.gallery.items.length">
          <!-- 阈值一旦生效，星星靠颜色表达不够明确，用文字再标一次 -->
          <span
            v-if="minStars"
            class="shrink-0 whitespace-nowrap rounded-md bg-amber-soft px-1.5 py-0.5 text-[11px] font-medium text-amber-text"
            :title="`只看 ≥${minStars}★ `"
          >≥{{ minStars }}★</span>
          <span class="shrink-0 whitespace-nowrap font-medium">{{ displayItems.length.toLocaleString() }} 项</span>
          <!-- 以下三项可收缩：空间不足时按「进度 → 大小 → 分母」依次让位，
               truncate 成省略号，不会出现半截字被硬裁；「N 项」与星级徽标刚性保留。
               工具条更窄时由 @container 直接隐藏（stat-secondary），把宽度全让给控件 -->
          <span v-if="kindFilter !== 'all' || ratingFilter !== 'all' || favoriteFilter" class="stat-secondary min-w-0 truncate text-xs text-ink-3" :class="{ 'is-narrow-hide': toolbarNarrow }">
            / 共 {{ store.gallery.items.length.toLocaleString() }} 项
          </span>
          <div v-if="totalSize" class="stat-secondary flex min-w-0 shrink-[10] items-center gap-x-3 text-ink-2" :class="{ 'is-narrow-hide': toolbarNarrow }">
            <span class="shrink-0 text-ink-3">·</span>
            <span class="min-w-0 truncate">{{ formatBytes(totalSize) }}</span>
          </div>
          <span v-if="timesProgress" class="stat-secondary flex min-w-0 shrink-[100] items-center gap-1.5 text-xs text-ink-2" :class="{ 'is-narrow-hide': toolbarNarrow }">
            <LoaderCircle class="size-3.5 shrink-0 animate-spin" />
            <span class="truncate">正在读取拍摄时间与评分 {{ timesProgress }}（首次较慢）</span>
          </span>
        </template>
        <span v-else class="shrink-0 whitespace-nowrap text-ink-2">图库</span>
        <span v-if="openError" class="max-w-64 shrink-0 truncate text-xs text-bad" :title="openError">{{ openError }}</span>
      </div>
      <div v-if="store.gallery.items.length" class="flex shrink-0 items-center gap-1.5">
        <div class="flex h-7 items-center gap-0.5 rounded-lg bg-subtle p-0.5">
          <button
            v-for="opt in filterOpts"
            :key="opt.value"
            class="flex h-6 shrink-0 items-center gap-1 whitespace-nowrap rounded-md px-2 text-xs transition-colors"
            :class="kindFilter === opt.value ? 'bg-surface text-ink shadow-sm' : 'text-ink-2 hover:text-ink'"
            :title="opt.value === 'all' ? '全部文件' : `${opt.label} ${(kindCounts[opt.value] ?? 0).toLocaleString()} 项`"
            @click="kindFilter = opt.value"
          >
            <component :is="opt.icon" class="size-3" />
            {{ opt.label }}
            <span v-if="opt.value !== 'all'" class="count-badge text-[10px] text-ink-3" :class="{ 'is-narrow-hide': toolbarNarrow }">{{ (kindCounts[opt.value] ?? 0).toLocaleString() }}</span>
          </button>
        </div>

        <span class="h-4 w-px shrink-0 bg-line"></span>

        <!-- 评分筛选：星级阈值（点第 N 颗 = ≥N★），评分覆盖全部媒体格式 -->
        <div
          class="flex h-7 items-center gap-0.5 rounded-lg bg-subtle p-0.5"
          :title="minStars ? `显示 ≥${minStars}★ 的文件（再点同一颗星取消）` : '按星级筛选（评分覆盖全部媒体格式）'"
          @mouseleave="hoverStars = 0"
        >
          <button
            class="h-6 shrink-0 whitespace-nowrap rounded-md px-1.5 text-[11px] transition-colors"
            :class="ratingFilter === 'all' ? 'bg-surface font-medium text-ink shadow-sm' : 'text-ink-2 hover:text-ink'"
            @click="ratingFilter = 'all'"
          >
            不限
          </button>
          <!-- 星级阈值：点第 N 颗 = ≥N★（含 N 星），悬停预览 -->
          <div class="mx-0.5 flex items-center gap-px border-x border-line px-1">
            <button
              v-for="n in 5"
              :key="n"
              class="grid size-[18px] place-items-center rounded transition-colors hover:bg-hover"
              :title="`≥${n}★`"
              @mouseenter="hoverStars = n"
              @click="setMinStars(n)"
            >
              <Star
                class="size-3 fill-current transition-colors"
                :style="starStyle(n, n <= starPreview)"
              />
            </button>
          </div>
          <button
            class="h-6 shrink-0 whitespace-nowrap rounded-md px-1.5 text-[11px] transition-colors"
            :class="ratingFilter === 'unrated' ? 'bg-surface font-medium text-ink shadow-sm' : 'text-ink-2 hover:text-ink'"
            @click="ratingFilter = 'unrated'"
          >
            未评
          </button>
        </div>

        <span class="h-4 w-px shrink-0 bg-line"></span>

        <!-- 收藏筛选：与类型/评分同样的「底槽 + 白药丸选中」，不再只靠图标变色 -->
        <div class="flex h-7 items-center rounded-lg bg-subtle p-0.5">
          <button
            class="flex h-6 shrink-0 items-center gap-1.5 whitespace-nowrap rounded-md px-2 text-xs transition-colors"
            :class="favoriteFilter ? 'bg-surface font-medium text-ink shadow-sm' : 'text-ink-2 hover:text-ink'"
            :title="favoriteFilter ? `只看收藏（${favoriteCount.toLocaleString()} 项，再点取消）` : `只看已收藏的文件（${favoriteCount.toLocaleString()} 项）`"
            :aria-pressed="favoriteFilter"
            @click="favoriteFilter = !favoriteFilter"
          >
            <Heart
              class="size-3.5"
              :class="favoriteFilter ? 'fill-current' : ''"
              :style="favoriteFilter ? { color: 'var(--fav-on)' } : undefined"
            />
            <span>收藏</span>
            <span class="count-badge text-[10px] text-ink-2 tabular-nums" :class="{ 'is-narrow-hide': toolbarNarrow }">{{ favoriteCount.toLocaleString() }}</span>
          </button>
        </div>

        <!-- 缩略图尺寸：离散 3 档；菜单 Teleport 到 body + fixed，按按钮 rect 定位 -->
        <div class="relative" data-view-menu>
          <button
            ref="viewBtn"
            class="flex h-7 shrink-0 items-center gap-1.5 whitespace-nowrap rounded-lg px-2 text-xs text-ink-2 transition-colors hover:bg-hover"
            :class="{ 'bg-hover text-ink': viewMenuOpen }"
            :title="`缩略图大小：${density.label}（${density.thumb}px）`"
            :aria-expanded="viewMenuOpen"
            @click="toggleViewMenu"
          >
            <LayoutGrid class="size-3.5" />
            <span class="view-label" :class="{ 'is-narrow-hide': toolbarNarrow }">{{ density.label }}</span>
            <ChevronDown class="size-3 transition-transform" :class="{ 'rotate-180': viewMenuOpen }" />
          </button>
        </div>

        <Teleport to="body">
          <div
            v-if="viewMenuOpen"
            class="fixed z-50 w-36 rounded-card border border-line bg-surface p-1 shadow-card"
            :style="{ top: menuPos.top + 'px', left: menuPos.left + 'px' }"
            data-view-menu
          >
            <button
              v-for="(d, i) in DENSITIES"
              :key="d.id"
              class="flex w-full items-center gap-2 rounded-lg px-2.5 py-1.5 text-left text-xs transition-colors hover:bg-hover"
              :class="i === densityIdx ? 'bg-amber-soft font-medium text-ink' : 'text-ink-2'"
              @click="setDensity(i)"
            >
              <LayoutGrid class="shrink-0" :style="{ width: d.icon + 'px', height: d.icon + 'px' }" />
              <span>{{ d.label }}</span>
              <span class="ml-auto text-[10px] text-ink-3">{{ d.thumb }}px</span>
              <Check v-if="i === densityIdx" class="size-3.5 shrink-0 text-amber" />
            </button>
          </div>
        </Teleport>
      </div>
      <!-- 信息栏开合 / 刷新：纯图标，省下约 70px 给筛选控件（含义走 title + aria-label） -->
      <button
        class="grid size-7 shrink-0 place-items-center rounded-lg transition-colors"
        :class="panelOpen ? 'text-ink-2 hover:bg-hover' : 'bg-hover text-ink'"
        :title="panelOpen ? '收起信息栏' : '展开信息栏'"
        :aria-label="panelOpen ? '收起信息栏' : '展开信息栏'"
        :aria-pressed="panelOpen"
        @click="togglePanel"
      >
        <component :is="panelOpen ? PanelRightClose : PanelRight" class="size-4" />
      </button>
      <button
        class="grid size-7 shrink-0 place-items-center rounded-lg text-ink-2 transition-colors hover:bg-hover disabled:opacity-50"
        :disabled="store.gallery.loading"
        title="刷新图库（重新扫描媒体库）"
        aria-label="刷新图库"
        @click="load"
      >
        <RefreshCw class="size-4" :class="{ 'animate-spin': store.gallery.loading }" />
      </button>
    </div>

    <!-- 网格区（左列，工具条下方） -->
    <div class="flex min-h-0 min-w-0 flex-col">
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
          <p class="text-sm text-ink-2">当前筛选下没有文件</p>
          <p v-if="emptyHint" class="mt-1.5 text-xs text-ink-2">{{ emptyHint }}</p>
          <button
            class="mt-3 rounded-lg border border-line px-3 py-1.5 text-xs text-ink-2 transition-colors hover:bg-hover"
            @click="clearFilters"
          >
            清除筛选
          </button>
        </div>
      </div>

      <!-- 虚拟网格：包装层不滚动，吸顶条才能绝对定位到工具条底边 -->
      <div class="relative min-h-0 flex-1">
        <div
          ref="scrollEl"
          class="absolute inset-0 overflow-y-auto px-6 pb-6 pt-4"
          @pointerdown="onPointerDown"
          @scroll.passive="onGridScroll"
          @contextmenu.prevent="onAreaContextMenu"
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
                  height: rowH + GAP + 'px',
                  display: 'grid',
                  gridTemplateColumns: `repeat(${cols}, minmax(0, 1fr))`,
                  gap: GAP + 'px',
                }"
              >
                <div
                  v-for="(item, localIdx) in rowAt(vrow.index).items"
                  :key="item.path"
                  data-cell
                  class="group cursor-default rounded-xl p-1 transition-colors"
                  :class="selected.has(item.path) ? 'bg-amber-soft ring-1 ring-amber' : 'hover:bg-hover'"
                  @click="onCellClick(item, rowAt(vrow.index).startIndex + localIdx, $event)"
                  @dblclick="openItem(item)"
                  @contextmenu="onCellContextMenu(item, rowAt(vrow.index).startIndex + localIdx, $event)"
                >
                  <div class="relative overflow-hidden rounded-lg bg-subtle" :style="{ height: thumbH + 'px' }">
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
                    <!-- 左下角标：格式与评分并排，共用同一 overlay 底色。
                         评分已放开到全格式，两个角标必须同容器，否则会叠在同一位置 -->
                    <div class="absolute bottom-1 left-1 flex items-center gap-1">
                      <span
                        v-if="item.kind === 'video'"
                        class="flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] text-white"
                        style="background-color: var(--badge-bg)"
                      >
                        <Film class="size-3" />
                        {{ item.ext.toUpperCase() }}
                      </span>
                      <span
                        v-if="item.rating"
                        class="flex items-center gap-0.5 rounded px-1.5 py-0.5 text-[10px] font-medium text-white"
                        style="background-color: var(--badge-bg)"
                      >
                        <Star
                          class="size-3 fill-current"
                          :style="{ color: starColor(item.rating) }"
                        />
                        {{ item.rating }}
                      </span>
                    </div>
                    <!-- 右下收藏心：已收藏常驻实心；未收藏仅 hover / 选中时出现空心 -->
                    <button
                      class="absolute bottom-1 right-1 grid size-6 place-items-center rounded-full transition-opacity"
                      :class="item.favorite || selected.has(item.path)
                        ? 'opacity-100 pointer-events-auto'
                        : 'opacity-0 pointer-events-none group-hover:opacity-100 group-hover:pointer-events-auto'"
                      :style="{ backgroundColor: item.favorite ? 'var(--badge-bg)' : 'rgba(30,26,22,.35)' }"
                      :title="item.favorite ? '取消收藏（空格）' : '收藏（空格）'"
                      :aria-pressed="item.favorite"
                      @pointerdown.stop
                      @click.stop="onHeartClick(item, $event)"
                      @dblclick.stop
                    >
                      <Heart
                        class="size-3.5 transition-colors"
                        :class="item.favorite ? 'fill-current' : ''"
                        :style="{ color: item.favorite ? 'var(--fav-on)' : 'var(--star-off)' }"
                        :stroke-width="item.favorite ? 0 : 2"
                      />
                    </button>
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

        <!-- 月份吸顶条：absolute 相对「不滚动」的包装层 → 紧贴工具条，
             把 pt-4 那条会漏出上一行照片的 16px 也一并盖住。
             overflow-hidden 自己裁掉被下一组顶出的部分；pointer-events-none 让框选/点击穿透。 -->
        <div
          v-if="stickyHeader"
          class="pointer-events-none absolute inset-x-6 top-0 z-20 overflow-hidden"
          :style="{ height: HEADER_H + 'px' }"
          aria-hidden="true"
        >
          <div
            class="flex items-center gap-2 bg-app pt-3"
            :style="{ transform: `translateY(${stickyShift}px)`, height: HEADER_H + 'px' }"
          >
            <span class="text-sm font-semibold">{{ stickyHeader.label }}</span>
            <span class="text-xs text-ink-3">{{ stickyHeader.count.toLocaleString() }} 项</span>
            <span class="ml-1 h-px flex-1 bg-line"></span>
          </div>
        </div>
      </div>
    </div>

    <!-- 右侧信息栏：单选看详情、多选看批量操作，动作卡位置固定不跳 -->
    <aside
      v-if="panelOpen"
      class="flex min-h-0 flex-col overflow-y-auto border-l border-line bg-surface"
    >
      <!-- 未选中：只放提示与快捷键，不做任何"假操作" -->
      <div v-if="!selected.size" class="grid flex-1 place-items-center px-6">
        <div class="text-center">
          <p class="text-xs leading-5 text-ink-2">选中一张照片或视频查看信息</p>
          <p v-if="displayItems.length" class="mt-3 text-[11px] leading-5 text-ink-2">
            方向键选择 · Shift 扩选 · Enter 打开<br />
            空格收藏 · 0-5 打分 · Ctrl+A 全选 · Esc 取消
          </p>
        </div>
      </div>

      <template v-else>
        <div class="border-b border-line px-5 pb-4 pt-5">
          <div class="mb-1.5 text-xs text-ink-3">
            {{ singleItem ? `${kindLabel(singleItem)} · ${singleItem.ext.toUpperCase()}` : "批量操作" }}
          </div>
          <div class="break-all text-sm font-medium leading-5" :title="singleItem?.name">
            {{ singleItem ? singleItem.name : `已选 ${selected.size} 项` }}
          </div>
        </div>

        <!-- 评分 / 收藏：单选与多选共用同一张卡，永远在顶部同一个位置 -->
        <div class="px-5 py-3">
          <div class="rounded-card border border-line bg-subtle px-3 py-2 text-sm">
            <div class="flex items-center justify-between gap-2">
              <span class="shrink-0 text-xs text-ink-2">评分</span>
              <div class="flex items-center gap-0.5" @mouseleave="hoverRating = 0">
                <button
                  v-for="i in 5"
                  :key="i"
                  class="rounded p-0.5 transition-transform"
                  :class="{
                    'scale-110': hoverRating === i,
                    'opacity-50': ratingBusy,
                  }"
                  :disabled="ratingBusy"
                  :title="i === curRating ? '清除评分' : `${i} 星`"
                  @mouseenter="hoverRating = i"
                  @click="onRate(i)"
                >
                  <Star
                    class="size-[18px] fill-current transition-colors"
                    :style="starStyle(i, (hoverRating || curRating) >= i)"
                  />
                </button>
                <span class="ml-1 text-xs text-ink-2 tabular-nums">
                  {{ singleItem ? hoverRating || curRating || "未评分" : hoverRating || "—" }}
                </span>
              </div>
            </div>
            <div class="mt-1.5 flex items-center justify-between gap-2 border-t border-line pt-1.5">
              <span class="shrink-0 text-xs text-ink-2">收藏</span>
              <div class="flex items-center gap-2">
                <button
                  class="grid size-6 place-items-center rounded-lg border border-line bg-surface transition-colors hover:bg-hover disabled:opacity-50"
                  :disabled="favBusy"
                  :title="favActive ? '取消收藏（空格）' : '收藏（空格）'"
                  :aria-pressed="favActive"
                  @click="toggleFavTargets"
                >
                  <Heart
                    class="size-3.5 transition-colors"
                    :class="favActive ? 'fill-current' : ''"
                    :style="{ color: favActive ? 'var(--fav-on)' : 'var(--star-off)' }"
                    :stroke-width="favActive ? 0 : 2"
                  />
                </button>
                <span class="text-xs" :class="favActive ? 'text-ink' : 'text-ink-2'">
                  {{ favActive ? "已收藏" : "未收藏" }}
                </span>
              </div>
            </div>
            <!-- 两条错误极少同时出现，合并成一行，避免把卡片撑高两行 -->
            <p v-if="favError || ratingError" class="mt-1 text-xs text-bad">{{ favError || ratingError }}</p>
            <p v-else-if="ratingBusy || favBusy" class="mt-1 text-xs text-ink-2">正在保存…</p>
          </div>
        </div>

        <!-- 单选：元数据 -->
        <dl v-if="singleItem" class="flex flex-col gap-2.5 px-5 pb-3 text-sm">
          <div v-if="infoLoading" class="py-6 text-center text-ink-3">
            <LoaderCircle class="mx-auto size-5 animate-spin" />
          </div>
          <template v-else>
            <div>
              <dt class="mb-0.5 text-xs text-ink-2">拍摄时间</dt>
              <dd class="text-ink">{{ info?.shootTime ?? "—" }}</dd>
            </div>
            <div>
              <dt class="mb-0.5 text-xs text-ink-2">相机</dt>
              <dd class="text-ink">{{ cameraLabel(info) ?? "—" }}</dd>
            </div>
            <div v-if="info?.lens">
              <dt class="mb-0.5 text-xs text-ink-2">镜头</dt>
              <dd class="break-words text-ink">{{ info.lens }}</dd>
            </div>
            <!-- 曝光四件套：两列排布，缺项自动不占位（视频/无 EXIF 时不显示整块） -->
            <div v-if="exposureRows.length" class="grid grid-cols-2 gap-x-3 gap-y-2.5">
              <div v-for="row in exposureRows" :key="row.label">
                <dt class="mb-0.5 text-xs text-ink-2">{{ row.label }}</dt>
                <dd class="text-ink">
                  {{ row.value }}
                  <span v-if="row.sub" class="block text-[11px] leading-4 text-ink-3">{{ row.sub }}</span>
                </dd>
              </div>
            </div>
            <div>
              <dt class="mb-0.5 text-xs text-ink-2">分辨率</dt>
              <dd class="text-ink">{{ info?.width && info?.height ? `${info.width} × ${info.height}` : "—" }}</dd>
            </div>
            <div>
              <dt class="mb-0.5 text-xs text-ink-2">大小</dt>
              <dd class="text-ink">{{ formatBytes(singleItem.size) }}</dd>
            </div>
            <div>
              <dt class="mb-0.5 text-xs text-ink-2">修改时间</dt>
              <dd class="text-ink">{{ fmtDate(singleItem.mtimeSecs) }}</dd>
            </div>
            <div>
              <dt class="mb-0.5 text-xs text-ink-2">位置</dt>
              <dd class="break-all text-ink-2">{{ relDir(singleItem) }}</dd>
            </div>
          </template>
        </dl>
        <p v-else class="px-5 pb-3 text-xs leading-5 text-ink-2">
          评分与收藏会同时写入这 {{ rateTargets.length }} 个文件；只写本地数据库，不动照片。
        </p>

        <div class="mt-auto border-t border-line p-4">
          <!-- 快捷键常驻：选中后信息栏一直有这条，不用先按 Esc 清空选择才能复习键位 -->
          <p class="mb-2 text-center text-[11px] leading-4 text-ink-3">
            {{
              singleItem
                ? "0-5 打分 · 空格收藏 · Enter 打开 · Esc 取消"
                : "0-5 批量打分 · 空格收藏 · Esc 取消"
            }}
          </p>
          <button
            v-if="singleItem"
            class="flex h-8 w-full items-center justify-center gap-1.5 rounded-lg bg-subtle text-xs text-ink-2 transition-colors hover:bg-hover"
            @click="invoke('open_in_explorer', { path: singleItem.path })"
          >
            <FolderOpen class="size-3.5" />
            打开所在文件夹
          </button>
        </div>
      </template>
    </aside>

    <!-- 网格右键菜单：Teleport 到 body + fixed，不受网格滚动与虚拟行 transform 裁切 -->
    <Teleport to="body">
      <div
        v-if="ctxOpen"
        class="fixed z-50 rounded-card border border-line bg-surface p-1 shadow-card"
        :style="{ top: ctxPos.top + 'px', left: ctxPos.left + 'px', width: CTX_W + 'px' }"
        data-ctx-menu
        @contextmenu.prevent
      >
        <div class="truncate px-2.5 py-1 text-[11px] text-ink-3" :title="ctxTitle">
          {{ ctxTooMany ? `一次最多打开 ${MAX_CTX_OPEN} 项` : ctxTitle }}
        </div>
        <div class="my-1 h-px bg-line"></div>
        <button
          class="flex w-full items-center gap-2 rounded-lg px-2.5 py-1.5 text-left text-xs text-ink-2 transition-colors"
          :class="ctxTooMany ? 'cursor-not-allowed opacity-45' : 'hover:bg-hover'"
          :disabled="ctxTooMany"
          @click="ctxOpenDefault"
        >
          <ExternalLink class="size-3.5 shrink-0" />
          <span>打开</span>
          <span v-if="ctxItems.length > 1" class="ml-auto text-[10px] text-ink-3">{{ ctxItems.length }} 项</span>
        </button>
        <button
          class="flex w-full items-center gap-2 rounded-lg px-2.5 py-1.5 text-left text-xs text-ink-2 transition-colors"
          :class="ctxTooMany ? 'cursor-not-allowed opacity-45' : 'hover:bg-hover'"
          :disabled="ctxTooMany"
          @click="ctxOpenPhotoshop"
        >
          <WandSparkles class="size-3.5 shrink-0" />
          <span>使用 Photoshop 打开</span>
          <span v-if="ctxItems.length > 1" class="ml-auto text-[10px] text-ink-3">{{ ctxItems.length }} 项</span>
        </button>
        <div class="my-1 h-px bg-line"></div>
        <button
          class="flex w-full items-center gap-2 rounded-lg px-2.5 py-1.5 text-left text-xs text-ink-2 transition-colors hover:bg-hover"
          @click="ctxOpenFolder"
        >
          <FolderOpen class="size-3.5 shrink-0" />
          <span>打开所在文件夹</span>
        </button>
      </div>
    </Teleport>
  </div>
</template>

<style scoped>
/* 工具条放不下时横向滚动而不是换行，隐藏细滚动条避免占掉 44px 高度 */
.toolbar {
  scrollbar-width: none;
}
.toolbar::-webkit-scrollbar {
  display: none;
}

/* 工具条内容宽 < 900px 时由 toolbarNarrow（ResizeObserver）挂上此类：
   低价值信息让位，保证类型 / 评分 / 收藏 / 查看 / 信息 / 刷新全部同行可见。
   阈值按工具条实际布局宽度判断，与侧栏 / 视口宽度解耦。
   信息没丢：分类计数、收藏数、缩略图档位分别在各自按钮的 title 里。 */
.is-narrow-hide {
  display: none !important; /* 压过 .flex 等 display 工具类 */
}
</style>
