import { reactive } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

/**
 * 全局状态（单例 reactive，不引 Pinia）
 * 页面切换用 store.page，模块间只通过这里与 invoke 交互
 */
export const store = reactive({
  page: null, // 'copy' | 'gallery' | 'settings'；启动时按规则决定（见 App.vue pickStartPage）
  config: null, // AppConfig {libraryRoot, originalDir, videoDir, pendingDir, lastPage, autoScan}
  stats: null, // LibraryStats {totalFiles, totalBytes, freeBytes}
  devices: [],
  devicesLoading: false,
  // 首页空态（无卡时）：最近月份统计 + 最近导入
  home: { data: null, loading: false, loaded: false, error: null },
  // 插卡自动扫描：已自动扫描过的设备路径（拔卡重置；跨页面重建仍有效）
  autoScannedDevice: null,
  // 拷贝整理页
  selectedDevice: null,
  plan: null,
  scanning: false,
  scanProgress: null, // {done, total, current}
  copying: false,
  paused: false,
  progress: null, // {doneFiles, doneBytes, totalFiles, totalBytes, current, bps, eta}
  report: null, // {copied, failed, elapsed, avgBps}
  error: null,
  // 图库页
  gallery: {
    items: [], // GalleryItem[]（初始按路径排序，时间补齐后按拍摄时间降序重排）
    loading: false,
    timesDone: 0, // 元数据（拍摄时间+评分）补齐进度
    timesTotal: 0,
  },
});

export function formatBytes(n) {
  if (n == null) return "—";
  if (n >= 1024 ** 3) return (n / 1024 ** 3).toFixed(2) + " GB";
  if (n >= 1024 ** 2) return (n / 1024 ** 2).toFixed(1) + " MB";
  if (n >= 1024) return (n / 1024).toFixed(1) + " KB";
  return n + " B";
}

export function formatEta(secs) {
  if (secs == null) return "—";
  const m = Math.floor(secs / 60);
  const s = Math.round(secs % 60);
  return m > 0 ? `${m} 分 ${s} 秒` : `${s} 秒`;
}

/** 统一错误提取：AppError 序列化为 {kind, message} */
export function errMsg(e) {
  if (typeof e === "string") return e;
  return e?.message || String(e);
}

export async function loadConfig() {
  store.config = await invoke("get_config");
}

export async function loadStats() {
  try {
    store.stats = await invoke("library_stats");
  } catch (e) {
    store.stats = null;
  }
}

export async function refreshDevices() {
  store.devicesLoading = true;
  try {
    store.devices = await invoke("list_devices");
  } finally {
    store.devicesLoading = false;
  }
}

/** 监听后端推送的库统计更新（拷贝结束自动刷新） */
export function watchLibraryStats() {
  listen("library-stats", (ev) => {
    store.stats = ev.payload;
  });
}

// ── 首页空态 ─────────────────────────────────────────────────────────────

/** 清除首页缓存（改库根 / 拷贝完成后需要重取） */
export function resetHome() {
  store.home = { data: null, loading: false, loaded: false, error: null };
}

/** 首页空态数据：无卡时展示；只在未加载过时请求一次，失败不自动重试 */
export async function loadHome() {
  if (store.home.loading || store.home.loaded) return;
  store.home.loading = true;
  store.home.error = null;
  try {
    store.home.data = await invoke("home_overview");
  } catch (e) {
    store.home.data = null;
    store.home.error = errMsg(e);
  } finally {
    store.home.loading = false;
    store.home.loaded = true;
  }
}

/** 是否有可用的卡（有卡的设备） */
export function hasCard() {
  return store.devices.some((d) => d.hasMedia);
}

// ── 图库 ─────────────────────────────────────────────────────────────────

/** 图库排序：拍摄时间降序（新→旧），无时间的排最后按路径 */
export function sortGalleryItems() {
  store.gallery.items.sort((a, b) => {
    if (a.shootTime && b.shootTime) {
      return b.shootTime.localeCompare(a.shootTime) || a.path.localeCompare(b.path);
    }
    if (a.shootTime) return -1;
    if (b.shootTime) return 1;
    return a.path.localeCompare(b.path);
  });
}

export async function loadGallery() {
  store.gallery.loading = true;
  try {
    store.gallery.items = await invoke("gallery_list");
    sortGalleryItems();
  } finally {
    store.gallery.loading = false;
  }
}

let galleryTimesWatched = false;

/** 监听元数据补齐事件（拍摄时间+评分）：合并补丁，全部完成后一次性重排 */
export function watchGalleryTimes() {
  if (galleryTimesWatched) return;
  galleryTimesWatched = true;
  listen("gallery-times", (ev) => {
    const { done, total, patches } = ev.payload;
    const map = new Map(patches.map((p) => [p.path, p]));
    for (const it of store.gallery.items) {
      const p = map.get(it.path);
      if (!p) continue;
      // 只补拍摄时间：评分来自 `ratings` 表（gallery_list 已带值），
      // 时间补丁不再含 rating 字段，写 it.rating 会把评分抹成 undefined
      it.shootTime = p.shootTime;
    }
    store.gallery.timesDone = done;
    store.gallery.timesTotal = total;
    if (done >= total) {
      store.gallery.timesDone = 0;
      store.gallery.timesTotal = 0;
      sortGalleryItems();
    }
  });
}

let galleryChangedWatched = false;

/** 监听媒体库文件系统变更（后端 2s 防抖聚合）：重拉列表实现热刷新 */
export function watchGalleryChanged() {
  if (galleryChangedWatched) return;
  galleryChangedWatched = true;
  listen("gallery-changed", () => {
    // 仅图库已加载过时重拉（未加载过由页面挂载触发首次加载）
    if (store.gallery.items.length && !store.gallery.loading) {
      loadGallery();
    }
  });
}
