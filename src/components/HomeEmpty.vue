<script setup>
import { computed, onMounted, ref } from "vue";
import { invoke } from "@tauri-apps/api/core";
import {
  Camera,
  RefreshCw,
  Images,
  FolderOpen,
  FileImage,
  Film,
  TriangleAlert,
} from "lucide-vue-next";
import { store, loadHome, formatBytes } from "../store.js";

const emit = defineEmits(["refresh"]);

const failed = ref(new Set());

const configured = computed(() => !!store.config?.libraryRoot);
const months = computed(() => store.home.data?.months ?? []);
const recent = computed(() => store.home.data?.recent ?? []);
const imports = computed(() => store.home.data?.imports ?? []);
const loading = computed(() => store.home.loading);
const showSetup = computed(() => !configured.value || !!store.home.error);
const emptyLib = computed(
  () => store.home.loaded && !store.home.error && !months.value.length && !recent.value.length
);

function thumbUrl(path) {
  return `http://thumb.localhost/${encodeURIComponent(path)}`;
}

function onThumbError(path) {
  const next = new Set(failed.value);
  next.add(path);
  failed.value = next;
}

function openFile(path) {
  invoke("gallery_open", { path }).catch(() => {});
}

function openDir(dir) {
  if (dir) invoke("open_in_explorer", { path: dir }).catch(() => {});
}

/** "J:\" → "J:" */
function shortDevice(path) {
  return (path || "").replace(/\\+$/, "");
}

/** 失败明细 → tooltip 文本 */
function failureText(rec) {
  if (!rec.failures?.length) return "";
  return rec.failures
    .slice(0, 10)
    .map((f) => `${f.fileName}：${f.reason}`)
    .join("\n");
}

onMounted(() => {
  // 只有确认配置了库才请求；未配置时后端必然报 LibraryUnreachable
  if (configured.value) loadHome();
});
</script>

<template>
  <div class="mx-auto flex w-full max-w-[820px] flex-col gap-5">
    <!-- 无卡提示 -->
    <div
      class="flex items-center gap-3 rounded-card border border-dashed border-line bg-surface/60 px-4 py-3.5"
    >
      <span class="grid size-9 shrink-0 place-items-center rounded-lg bg-subtle text-ink-3">
        <Camera class="size-4.5" />
      </span>
      <div class="leading-tight">
        <div class="text-sm font-medium">未检测到 SD 卡</div>
        <div class="text-xs text-ink-3">插入相机卡后会自动识别并扫描</div>
      </div>
      <button
        class="ml-auto flex items-center gap-1.5 rounded-lg px-2.5 py-1 text-xs text-ink-2 transition-colors hover:bg-hover"
        :class="{ 'pointer-events-none opacity-55': store.devicesLoading }"
        @click="emit('refresh')"
      >
        <RefreshCw class="size-3.5" :class="{ 'animate-spin': store.devicesLoading }" />
        刷新
      </button>
    </div>

    <!-- 未配置媒体库 / 库不可达 -->
    <section
      v-if="showSetup"
      class="rounded-card border border-line bg-surface px-6 py-8 text-center shadow-card"
    >
      <div class="mb-1 text-sm font-medium">
        {{ configured ? "媒体库无法访问" : "还没设置媒体库" }}
      </div>
      <p class="mx-auto mb-4 max-w-sm text-xs leading-relaxed text-ink-2">
        {{
          configured
            ? store.home.error
            : "选一个文件夹作为照片与视频的归档位置，之后每次插卡都会拷到这里"
        }}
      </p>
      <button
        class="h-9 rounded-lg bg-amber px-4 text-sm font-medium text-white shadow-sm transition-colors hover:bg-amber-deep"
        @click="store.page = 'settings'"
      >
        去设置
      </button>
    </section>

    <template v-else>
      <!-- 最近导入 -->
      <section class="space-y-3">
        <div class="flex items-baseline justify-between">
          <h3 class="flex items-center gap-1.5 text-sm font-semibold text-ink-2">
            最近导入
            <span v-if="loading && !recent.length" class="text-xs font-normal text-ink-3">
              加载中…
            </span>
          </h3>
          <span v-if="months[0]" class="text-xs text-ink-3">
            {{ months[0].label }} · {{ months[0].files.toLocaleString() }} 个文件 ·
            {{ formatBytes(months[0].bytes) }}
          </span>
        </div>

        <!-- 骨架：避免图片逐个到达时格子塌陷 -->
        <div v-if="loading && !recent.length" class="grid grid-cols-6 gap-2">
          <div
            v-for="i in 12"
            :key="i"
            class="aspect-square animate-pulse rounded-lg bg-subtle"
          />
        </div>

        <div v-else-if="recent.length" class="grid grid-cols-6 gap-2">
          <button
            v-for="it in recent"
            :key="it.path"
            class="group relative aspect-square overflow-hidden rounded-lg border border-line bg-subtle"
            :title="it.name"
            @click="openFile(it.path)"
          >
            <img
              v-if="!failed.has(it.path)"
              :src="thumbUrl(it.path)"
              class="size-full object-cover transition-opacity group-hover:opacity-85"
              loading="lazy"
              decoding="async"
              draggable="false"
              @error="onThumbError(it.path)"
            />
            <span v-else class="grid size-full place-items-center text-ink-3">
              <Film v-if="it.kind === 'video'" class="size-4" />
              <FileImage v-else class="size-4" />
            </span>
          </button>
        </div>

        <div
          v-else-if="emptyLib || (!loading && store.home.loaded)"
          class="flex items-center justify-center gap-2 rounded-card border border-dashed border-line bg-surface/60 px-4 py-10 text-sm text-ink-3"
        >
          <FileImage class="size-4" /> 库里还没有照片——插入相机卡导入第一批吧
        </div>
      </section>

      <!-- 月份归档 -->
      <section v-if="months.length" class="flex flex-wrap items-center gap-2">
        <span class="text-xs text-ink-3">按月归档</span>
        <button
          v-for="m in months"
          :key="m.label"
          class="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1 text-xs text-ink-2 transition-colors hover:border-line-strong hover:bg-hover"
          :title="m.dir"
          @click="openDir(m.dir)"
        >
          {{ m.label }}
          <span class="tabular-nums text-ink-3">{{ m.files.toLocaleString() }}</span>
        </button>
      </section>

      <!-- 导入记录 -->
      <section v-if="imports.length" class="space-y-2">
        <h3 class="text-sm font-semibold text-ink-2">最近导入记录</h3>
        <ul class="divide-y divide-line overflow-hidden rounded-card border border-line bg-surface">
          <li
            v-for="r in imports"
            :key="r.id"
            class="flex items-center gap-2.5 px-3.5 py-2 text-xs"
            :title="failureText(r)"
          >
            <span class="shrink-0 tabular-nums text-ink-3">{{ r.finishedAt }}</span>
            <span class="shrink-0 text-ink-3">{{ shortDevice(r.devicePath) }}</span>
            <span
              v-if="r.cancelled"
              class="shrink-0 rounded bg-warn-soft px-1.5 py-0.5 text-[11px] text-warn"
            >
              已取消
            </span>
            <span class="min-w-0 truncate text-ink-2">
              拷贝 <b class="font-medium tabular-nums text-ink">{{ r.filesCopied }}</b> 个 ·
              {{ formatBytes(r.bytesCopied) }}
              <template v-if="r.filesSkipped"> · 跳过 {{ r.filesSkipped }}</template>
              <template v-if="r.filesConflict">
                · <span class="text-warn">冲突 {{ r.filesConflict }}</span>
              </template>
              <template v-if="r.filesFailed">
                · <span class="text-bad">失败 {{ r.filesFailed }}</span>
              </template>
            </span>
            <span class="ml-auto shrink-0 tabular-nums text-ink-3">
              {{ r.elapsedSecs }} 秒 · {{ formatBytes(r.avgBytesPerSec) }}/s
            </span>
          </li>
        </ul>
      </section>

      <!-- 入口 -->
      <div class="mt-1 flex items-center gap-2">
        <button
          class="flex h-9 items-center gap-1.5 rounded-lg bg-amber px-4 text-sm font-medium text-white shadow-sm transition-colors hover:bg-amber-deep"
          @click="store.page = 'gallery'"
        >
          <Images class="size-4" /> 打开图库
        </button>
        <button
          class="flex h-9 items-center gap-1.5 rounded-lg border border-line px-4 text-sm text-ink-2 transition-colors hover:bg-hover"
          @click="openDir(store.config?.libraryRoot)"
        >
          <FolderOpen class="size-4" /> 打开媒体库文件夹
        </button>
      </div>

      <div v-if="store.home.error" class="flex items-center gap-1.5 text-xs text-bad">
        <TriangleAlert class="size-3.5" /> {{ store.home.error }}
      </div>
    </template>
  </div>
</template>
