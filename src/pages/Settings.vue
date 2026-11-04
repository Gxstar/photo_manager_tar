<script setup>
import { ref } from "vue";
import { open } from "@tauri-apps/plugin-dialog";
import { invoke } from "@tauri-apps/api/core";
import { FolderOpen, HardDrive, FileStack, Save, TriangleAlert } from "lucide-vue-next";
import {
  store,
  loadConfig,
  loadStats,
  errMsg,
  formatBytes,
} from "../store.js";

const saving = ref(false);
const error = ref(null);

async function pickLibraryRoot() {
  const dir = await open({
    directory: true,
    multiple: false,
    title: "选择媒体库根目录",
    defaultPath: store.config?.libraryRoot,
  });
  if (!dir) return;
  await saveRoot(dir);
}

async function saveRoot(dir) {
  saving.value = true;
  error.value = null;
  try {
    store.config = await invoke("set_library_root", { root: dir });
    await loadStats();
  } catch (e) {
    error.value = errMsg(e);
  } finally {
    saving.value = false;
  }
}
</script>

<template>
  <div class="mx-auto max-w-2xl space-y-4 p-6">
    <!-- 媒体库路径 -->
    <section class="rounded-card border border-line bg-surface p-5 shadow-card">
      <h2 class="mb-1 text-sm font-semibold">媒体库位置</h2>
      <p class="mb-3 text-xs text-ink-2">
        照片与视频将按拍摄月份归档到该目录下的「原图 / 视频」文件夹
      </p>
      <div class="flex items-center gap-2">
        <div
          class="flex h-9 min-w-0 flex-1 items-center gap-2 rounded-lg border border-line bg-subtle px-3 text-sm"
        >
          <FolderOpen class="size-4 shrink-0 text-ink-3" />
          <span class="truncate" :title="store.config?.libraryRoot">
            {{ store.config?.libraryRoot || "未设置" }}
          </span>
        </div>
        <button
          class="flex h-9 shrink-0 items-center gap-1.5 rounded-lg bg-amber px-3.5 text-sm font-medium text-white transition-colors hover:bg-amber-deep disabled:opacity-55"
          :disabled="saving"
          @click="pickLibraryRoot"
        >
          <Save class="size-4" />
          更改
        </button>
      </div>
      <p v-if="error" class="mt-2 flex items-center gap-1 text-xs text-bad">
        <TriangleAlert class="size-3.5" /> {{ error }}
      </p>
    </section>

    <!-- 库统计 -->
    <section
      v-if="store.stats"
      class="grid grid-cols-3 gap-3"
    >
      <div class="rounded-card border border-line bg-surface p-4 shadow-card">
        <div class="flex items-center gap-2 text-xs text-ink-2">
          <FileStack class="size-4 text-ink-3" /> 文件总数
        </div>
        <div class="mt-1.5 text-xl font-semibold tabular-nums">
          {{ store.stats.totalFiles.toLocaleString() }}
        </div>
      </div>
      <div class="rounded-card border border-line bg-surface p-4 shadow-card">
        <div class="flex items-center gap-2 text-xs text-ink-2">
          <HardDrive class="size-4 text-ink-3" /> 已占用
        </div>
        <div class="mt-1.5 text-xl font-semibold tabular-nums">
          {{ formatBytes(store.stats.totalBytes) }}
        </div>
      </div>
      <div class="rounded-card border border-line bg-surface p-4 shadow-card">
        <div class="flex items-center gap-2 text-xs text-ink-2">
          <HardDrive class="size-4 text-ink-3" /> 剩余空间
        </div>
        <div class="mt-1.5 text-xl font-semibold tabular-nums">
          {{ formatBytes(store.stats.freeBytes) }}
        </div>
      </div>
    </section>

    <!-- 归档规则说明 -->
    <section class="rounded-card border border-line bg-surface p-5 text-sm shadow-card">
      <h2 class="mb-2 text-sm font-semibold">归档规则</h2>
      <ul class="space-y-1.5 text-xs leading-relaxed text-ink-2">
        <li>· 文件保持相机原厂文件名，不做任何改名</li>
        <li>· 照片 / RAW → <code class="rounded bg-subtle px-1">原图\年份\年-月\</code>；视频 → <code class="rounded bg-subtle px-1">视频\年份\年-月\</code></li>
        <li>· 无法确定拍摄时间的文件 → <code class="rounded bg-subtle px-1">原图\待定\</code></li>
        <li>· 同名同大小的文件视为已存在，自动跳过；同名不同大小标为冲突，绝不覆盖</li>
        <li>· 拷贝只新增文件，永不修改或删除媒体库中已有内容</li>
      </ul>
    </section>
  </div>
</template>
