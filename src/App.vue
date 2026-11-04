<script setup>
import { computed, onMounted, watch } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { HardDrive, Settings as SettingsIcon, FolderOpen, Images } from "lucide-vue-next";
import {
  store,
  loadConfig,
  loadStats,
  refreshDevices,
  watchLibraryStats,
  hasCard,
  formatBytes,
} from "./store.js";
import CopyOrganize from "./pages/CopyOrganize.vue";
import Gallery from "./pages/Gallery.vue";
import Settings from "./pages/Settings.vue";

const pages = [
  { id: "copy", title: "拷贝整理", icon: HardDrive, component: CopyOrganize },
  { id: "gallery", title: "图库", icon: Images, component: Gallery },
  { id: "settings", title: "设置", icon: SettingsIcon, component: Settings },
];

const current = computed(() => pages.find((p) => p.id === store.page) ?? pages[0]);

// 侧边栏只有 ~160px 可用宽度，整条路径塞进去会把「媒体库」挤成一列竖排字。
// 只显示末段目录名，完整路径进 tooltip。
const rootLabel = computed(() => {
  const root = (store.config?.libraryRoot ?? "").replace(/[\\/]+$/, "");
  if (!root) return "未设置";
  const seg = root.split(/[\\/]/).filter(Boolean).pop();
  return seg ?? root;
});

/**
 * 启动落点：插着有内容的卡 → 直奔导入页（配合插卡自动扫描，插卡后零点击）；
 * 否则恢复上次停留页，首次启动落在拷贝整理
 */
function pickStartPage() {
  if (hasCard()) return "copy";
  const last = store.config?.lastPage;
  return pages.some((p) => p.id === last) ? last : "copy";
}

onMounted(async () => {
  watchLibraryStats();
  // 必须先拿到设备列表再决策，否则会把「有卡」误判成「无卡」，页面闪一下再跳走
  await loadConfig();
  await refreshDevices();
  store.page = pickStartPage();
  loadStats(); // 全库统计较慢，不阻塞首屏（侧边栏数据异步补齐）
});

// 记录上次停留页，供下次启动恢复。
// 首次触发是启动落点（程序决定，不算用户选择），跳过以免把「有卡强制导入」写回配置
let pageDecided = false;
watch(
  () => store.page,
  (p) => {
    if (!p) return;
    if (!pageDecided) {
      pageDecided = true;
      return;
    }
    invoke("set_last_page", { page: p }).catch(() => {});
  }
);
</script>

<template>
  <div class="flex h-full">
    <!-- 侧边栏 -->
    <aside class="flex w-60 shrink-0 flex-col border-r border-line bg-surface">
      <div class="flex items-center gap-2 px-5 pt-5 pb-4">
        <span class="grid size-8 place-items-center rounded-lg bg-amber-soft text-amber">
          <FolderOpen class="size-4.5" />
        </span>
        <div class="leading-tight">
          <div class="text-[15px] font-semibold">显影</div>
          <div class="text-[11px] text-ink-3">develop</div>
        </div>
      </div>

      <nav class="flex flex-col gap-1 px-3">
        <button
          v-for="p in pages"
          :key="p.id"
          class="relative flex h-9 items-center gap-2.5 rounded-lg px-3 text-sm transition-colors"
          :class="
            store.page === p.id
              ? 'bg-amber-soft font-medium text-ink'
              : 'text-ink-2 hover:bg-hover'
          "
          @click="store.page = p.id"
        >
          <span
            v-if="store.page === p.id"
            class="absolute left-0 top-1/2 h-4.5 w-[3px] -translate-y-1/2 rounded-full bg-amber"
          />
          <component :is="p.icon" class="size-4" />
          {{ p.title }}
        </button>
      </nav>

      <div class="mt-auto px-4 pb-4">
        <div
          v-if="store.stats"
          class="rounded-xl bg-subtle px-3.5 py-3 text-xs text-ink-2"
        >
          <div class="mb-1.5 flex items-center justify-between gap-2">
            <span class="shrink-0">媒体库</span>
            <span class="min-w-0 truncate text-ink-3" :title="store.config?.libraryRoot">
              {{ rootLabel }}
            </span>
          </div>
          <div class="truncate">{{ store.stats.totalFiles.toLocaleString() }} 个文件 · {{ formatBytes(store.stats.totalBytes) }}</div>
          <div class="mt-0.5 text-ink-3">剩余 {{ formatBytes(store.stats.freeBytes) }}</div>
        </div>
      </div>
    </aside>

    <!-- 主区 -->
    <main class="flex min-w-0 flex-1 flex-col">
      <header class="flex h-14 shrink-0 items-center border-b border-line bg-surface px-6">
        <h1 class="text-base font-semibold">{{ current.title }}</h1>
      </header>
      <div class="min-h-0 flex-1 overflow-y-auto">
        <Transition name="fade" mode="out-in">
          <component :is="current.component" v-if="store.page" />
        </Transition>
      </div>
    </main>
  </div>
</template>

<style scoped>
.fade-enter-active,
.fade-leave-active {
  transition: opacity 0.15s ease;
}
.fade-enter-from,
.fade-leave-to {
  opacity: 0;
}
</style>
