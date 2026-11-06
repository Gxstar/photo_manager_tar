<script setup>
import { computed, onMounted, ref } from "vue";
// window.confirm 在该 WebView 返回 Promise（恒真），必须用原生对话框插件
import { ask, open } from "@tauri-apps/plugin-dialog";
import { invoke } from "@tauri-apps/api/core";
import { FolderOpen, HardDrive, FileStack, Save, TriangleAlert } from "lucide-vue-next";
import {
  store,
  loadConfig,
  loadStats,
  errMsg,
  resetHome,
  loadGallery,
  formatBytes,
} from "../store.js";

const saving = ref(false);
const error = ref(null);
const behaviorError = ref(null);

const autoScan = computed(() => store.config?.autoScan !== false);

const clearing = ref(false);
const clearedMsg = ref(null);

// ── 评分与互操作（SQLite 权威 + XMP sidecar 手动导出）──────────────────────
const interopBusy = ref(false);
const interopError = ref(null);
const interopMsg = ref(null);
const dbPath = ref("");

onMounted(async () => {
  try {
    dbPath.value = await invoke("ratings_db_path");
  } catch {
    dbPath.value = "";
  }
});

async function importRatings() {
  const ok = await ask(
    "从照片文件内嵌 XMP 导入旧评分？只填补本地还没有评分的文件，不覆盖已打的分；全程只读文件、不修改照片。",
    { title: "导入评分", kind: "info", okLabel: "导入", cancelLabel: "取消" },
  );
  if (!ok) return;
  interopBusy.value = true;
  interopError.value = null;
  interopMsg.value = null;
  try {
    const r = await invoke("ratings_import_from_files");
    interopMsg.value = `导入 ${r.imported} 项 · 已有评分跳过 ${r.skippedExisting} 项 · 文件里无评分 ${r.noRating} 项 · 无法读取 ${r.unreadable} 项（厂商 RAW / 视频等不读内嵌 XMP，属预期）`;
    loadGallery().catch(() => {}); // 让图库切回来时角标与筛选立刻反映新值
  } catch (e) {
    interopError.value = errMsg(e);
  } finally {
    interopBusy.value = false;
  }
}

async function exportSidecars() {
  const ok = await ask(
    "把评分导出成 XMP sidecar？会在媒体库目录新增或更新 .xmp 文件，不修改任何照片。已存在的第三方 sidecar 一律跳过、绝不覆盖。",
    { title: "导出评分", kind: "warning", okLabel: "导出", cancelLabel: "取消" },
  );
  if (!ok) return;
  interopBusy.value = true;
  interopError.value = null;
  interopMsg.value = null;
  try {
    const r = await invoke("ratings_export_sidecars");
    interopMsg.value = `新建 ${r.written} 个 · 更新 ${r.updated} 个 · 跳过第三方 sidecar ${r.skipped} 个${r.failed.length ? ` · 失败 ${r.failed.length} 个` : ""}`;
    if (r.skippedFiles.length) interopMsg.value += `（跳过示例：${r.skippedFiles[0]}）`;
    if (r.failed.length) interopMsg.value += `（失败：${r.failed.slice(0, 3).join("；")}）`;
  } catch (e) {
    interopError.value = errMsg(e);
  } finally {
    interopBusy.value = false;
  }
}

async function clearLog() {
  const ok = await ask("清空全部导入记录？（只删记录，不影响照片与视频文件）", {
    title: "清空导入记录",
    kind: "warning",
    okLabel: "清空",
    cancelLabel: "取消",
  });
  if (!ok) return;
  clearing.value = true;
  behaviorError.value = null;
  clearedMsg.value = null;
  try {
    const n = await invoke("import_log_clear");
    clearedMsg.value = `已清空 ${n} 条记录`;
    resetHome(); // 首页的记录列表需要重取
  } catch (e) {
    behaviorError.value = errMsg(e);
  } finally {
    clearing.value = false;
  }
}

async function toggleAutoScan() {
  behaviorError.value = null;
  try {
    store.config = await invoke("set_auto_scan", { enabled: !autoScan.value });
  } catch (e) {
    behaviorError.value = errMsg(e);
  }
}

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
    resetHome(); // 库根变了，首页空态数据必须重取
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
          class="flex h-9 shrink-0 items-center gap-1.5 rounded-lg bg-amber px-3.5 text-sm font-medium text-ink ring-1 ring-inset ring-amber-deep/45 transition-colors hover:bg-amber-deep disabled:opacity-55"
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
          <FileStack class="size-4 text-ink-3" /> 媒体文件
        </div>
        <div class="mt-1.5 text-xl font-semibold tabular-nums">
          {{ store.stats.totalFiles.toLocaleString() }}
        </div>
        <div class="mt-0.5 text-[11px] text-ink-3">原图 + 视频，与图库项数一致</div>
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

    <!-- 行为 -->
    <section class="rounded-card border border-line bg-surface p-5 shadow-card">
      <h2 class="mb-3 text-sm font-semibold">行为</h2>
      <div class="flex items-center justify-between gap-4">
        <div class="min-w-0">
          <div class="text-sm">插卡后自动扫描</div>
          <p class="mt-0.5 text-xs leading-relaxed text-ink-2">
            检测到相机卡后自动生成归档计划。扫描只读卡、不改卡，仍需手动点「开始拷贝」
          </p>
        </div>
        <button
          role="switch"
          :aria-checked="autoScan ? 'true' : 'false'"
          aria-label="插卡后自动扫描"
          class="relative h-6 w-11 shrink-0 rounded-full transition-colors"
          :class="autoScan ? 'bg-amber' : 'bg-line-strong'"
          @click="toggleAutoScan"
        >
          <span
            class="absolute top-0.5 size-5 rounded-full bg-white shadow-sm transition-[left] duration-150"
            :style="{ left: autoScan ? '22px' : '2px' }"
          />
        </button>
      </div>
      <p v-if="behaviorError" class="mt-2 flex items-center gap-1 text-xs text-bad">
        <TriangleAlert class="size-3.5" /> {{ behaviorError }}
      </p>

      <div class="mt-4 flex items-center justify-between gap-4 border-t border-line pt-4">
        <div class="min-w-0">
          <div class="text-sm">导入记录</div>
          <p class="mt-0.5 text-xs leading-relaxed text-ink-2">
            每次拷贝结束自动记录一条（含失败明细与取消），无卡首页展示最近 5 条
            <span v-if="clearedMsg" class="text-ink-3">· {{ clearedMsg }}</span>
          </p>
        </div>
        <button
          class="h-8 shrink-0 rounded-lg border border-line px-3 text-xs text-ink-2 transition-colors hover:bg-hover disabled:opacity-55"
          :disabled="clearing"
          @click="clearLog"
        >
          清空记录
        </button>
      </div>
    </section>

    <!-- 评分与互操作 -->
    <section class="rounded-card border border-line bg-surface p-5 shadow-card">
      <h2 class="mb-1 text-sm font-semibold">评分与互操作</h2>
      <p class="mb-4 text-xs leading-relaxed text-ink-2">
        评分保存在本地数据库，永不修改照片文件。媒体库里的原图保持只读，编辑请另存副本。
      </p>
      <div class="flex items-center justify-between gap-4">
        <div class="min-w-0">
          <div class="text-sm">从文件导入旧评分</div>
          <p class="mt-0.5 text-xs leading-relaxed text-ink-2">
            一次性读取 JPEG / AVIF·HEIC 内嵌 XMP，只填补本地还没有评分的文件
          </p>
        </div>
        <button
          class="h-8 shrink-0 rounded-lg border border-line px-3 text-xs text-ink-2 transition-colors hover:bg-hover disabled:opacity-55"
          :disabled="interopBusy"
          @click="importRatings"
        >
          导入
        </button>
      </div>
      <div class="mt-4 flex items-center justify-between gap-4 border-t border-line pt-4">
        <div class="min-w-0">
          <div class="text-sm">导出评分到 XMP sidecar</div>
          <p class="mt-0.5 text-xs leading-relaxed text-ink-2">
            给 Bridge / Lightroom / darktable / XnView 等软件读取；默认不生成，第三方 sidecar 一律跳过
          </p>
        </div>
        <button
          class="h-8 shrink-0 rounded-lg border border-line px-3 text-xs text-ink-2 transition-colors hover:bg-hover disabled:opacity-55"
          :disabled="interopBusy"
          @click="exportSidecars"
        >
          导出
        </button>
      </div>
      <p v-if="interopMsg" class="mt-3 text-xs leading-5 text-ink-2">{{ interopMsg }}</p>
      <p v-if="interopError" class="mt-2 flex items-center gap-1 text-xs text-bad">
        <TriangleAlert class="size-3.5" /> {{ interopError }}
      </p>
      <p class="mt-3 border-t border-line pt-3 text-xs leading-5 text-ink-2">
        评分数据库：<span class="break-all">{{ dbPath || "—" }}</span>（换电脑 / 重装前请备份此文件）。
        Windows 资源管理器不读 sidecar，其「分级」列不会显示——这是保持原图只读的取舍。
      </p>
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
