<script setup>
import { computed, onMounted } from "vue";
import { HardDrive, Settings as SettingsIcon, FolderOpen } from "lucide-vue-next";
import { store, loadConfig, loadStats, refreshDevices, watchLibraryStats, formatBytes } from "./store.js";
import CopyOrganize from "./pages/CopyOrganize.vue";
import Settings from "./pages/Settings.vue";

const pages = [
  { id: "copy", title: "拷贝整理", icon: HardDrive, component: CopyOrganize },
  { id: "settings", title: "设置", icon: SettingsIcon, component: Settings },
];

const current = computed(() => pages.find((p) => p.id === store.page) ?? pages[0]);

onMounted(async () => {
  watchLibraryStats();
  await loadConfig();
  await loadStats();
  refreshDevices();
});
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
          <div class="text-[15px] font-semibold">SD 卡拷贝整理</div>
          <div class="text-[11px] text-ink-3">sdcard-importer</div>
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
          <div class="mb-1 flex items-center justify-between">
            <span>媒体库</span>
            <span class="text-ink-3">{{ store.config?.libraryRoot }}</span>
          </div>
          <div class="flex items-center justify-between">
            <span>{{ store.stats.totalFiles.toLocaleString() }} 个文件 · {{ formatBytes(store.stats.totalBytes) }}</span>
            <span class="text-ink-3">剩 {{ formatBytes(store.stats.freeBytes) }}</span>
          </div>
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
          <component :is="current.component" />
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
