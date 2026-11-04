import { reactive } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

/**
 * 全局状态（单例 reactive，不引 Pinia）
 * 页面切换用 store.page，模块间只通过这里与 invoke 交互
 */
export const store = reactive({
  page: "copy", // 'copy' | 'settings'
  config: null, // AppConfig {libraryRoot, originalDir, videoDir, pendingDir}
  stats: null, // LibraryStats {totalFiles, totalBytes, freeBytes}
  devices: [],
  devicesLoading: false,
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
