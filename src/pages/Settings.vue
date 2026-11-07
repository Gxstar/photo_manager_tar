<script setup>
import { computed, onMounted, ref } from "vue";
// window.confirm 在该 WebView 返回 Promise（恒真），必须用原生对话框插件
import { ask, open } from "@tauri-apps/plugin-dialog";
import { invoke } from "@tauri-apps/api/core";
import { Check, ChevronDown, Copy, FolderPen, RotateCcw, TriangleAlert } from "lucide-vue-next";
import ModelSettings from "../components/ModelSettings.vue";
import PlaceSettings from "../components/PlaceSettings.vue";
import SettingGroup from "../components/SettingGroup.vue";
import SettingRow from "../components/SettingRow.vue";
import {
  store,
  loadStats,
  errMsg,
  resetHome,
  loadGallery,
  formatBytes,
} from "../store.js";

/*
 * 按钮规格（全页唯一口径，改动请一起改，避免高度/内距漂移）：
 *   主按钮  h-8 + px-3 + text-xs + amber
 *   次按钮  h-8 + px-3 + text-xs + 描边
 *   图标钮  size-8 + 描边
 *   危险钮  h-8 + px-3 + text-xs + bad 描边
 */

const saving = ref(false);
const error = ref(null);
const scanError = ref(null);
const clearError = ref(null);

// 复制路径：给一次短反馈，避免"点了没反应"
const copied = ref(false);
let copiedTimer = 0;
const savedFlash = ref(false);
let savedTimer = 0;
const dbCopied = ref(false);
let dbCopiedTimer = 0;

async function copyRoot() {
  const root = store.config?.libraryRoot;
  if (!root) return;
  try {
    await navigator.clipboard.writeText(root);
    error.value = null;
    copied.value = true;
    clearTimeout(copiedTimer);
    copiedTimer = setTimeout(() => (copied.value = false), 1600);
  } catch {
    error.value = "复制失败，可直接选中路径文字手动复制";
  }
}

const autoScan = computed(() => store.config?.autoScan !== false);

const clearing = ref(false);
const clearedMsg = ref(null);

// ── 评分与互操作（SQLite 权威 + XMP sidecar 手动导出）──────────────────────
const interopBusy = ref(false);
const interopError = ref(null);
const interopMsg = ref(null);
const dbPath = ref("");

async function copyDbPath() {
  if (!dbPath.value) return;
  try {
    await navigator.clipboard.writeText(dbPath.value);
    interopError.value = null;
    dbCopied.value = true;
    clearTimeout(dbCopiedTimer);
    dbCopiedTimer = setTimeout(() => (dbCopied.value = false), 1600);
  } catch {
    interopError.value = "复制失败，请手动选中路径";
  }
}

// ── 外部编辑器（图库网格右键「使用 Photoshop 打开」）─────────────────────
const editorError = ref(null);
const editorBusy = ref(false);
const editorSaved = ref(false);
let editorSavedTimer = 0;
const detectedExe = ref("");

const configuredExe = computed(() => store.config?.photoshopPath || "");
const effectiveExe = computed(() => configuredExe.value || detectedExe.value);

// 路径放行描述位（一行），因此「自动检测」这类补充说明并进同一句
const editorDesc = computed(() => {
  if (configuredExe.value) return configuredExe.value;
  if (detectedExe.value) return `${detectedExe.value} · 自动检测`;
  return "未找到 Photoshop.exe，可手动指定一次";
});

async function refreshDetected() {
  try {
    detectedExe.value = (await invoke("detect_photoshop")) || "";
  } catch {
    detectedExe.value = "";
  }
}

async function savePhotoshopPath(path) {
  editorBusy.value = true;
  editorError.value = null;
  try {
    store.config = await invoke("set_photoshop_path", { path });
    await refreshDetected();
    editorSaved.value = true;
    clearTimeout(editorSavedTimer);
    editorSavedTimer = setTimeout(() => (editorSaved.value = false), 1600);
  } catch (e) {
    editorError.value = errMsg(e);
  } finally {
    editorBusy.value = false;
  }
}

async function pickPhotoshop() {
  const picked = await open({
    multiple: false,
    title: "选择 Photoshop.exe",
    filters: [{ name: "可执行文件", extensions: ["exe"] }],
  });
  const file = Array.isArray(picked) ? picked[0] : picked;
  if (file) await savePhotoshopPath(file);
}

onMounted(async () => {
  refreshDetected();
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
    interopMsg.value = `已导入 ${r.imported} 项 · 已有评分跳过 ${r.skippedExisting} 项 · 文件里无评分 ${r.noRating} 项 · 无法读取 ${r.unreadable} 项（厂商 RAW / 视频不读内嵌 XMP，属预期）`;
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
  clearError.value = null;
  clearedMsg.value = null;
  try {
    const n = await invoke("import_log_clear");
    clearedMsg.value = `已清空 ${n} 条记录`;
    resetHome(); // 首页的记录列表需要重取
  } catch (e) {
    clearError.value = errMsg(e);
  } finally {
    clearing.value = false;
  }
}

async function toggleAutoScan() {
  scanError.value = null;
  try {
    store.config = await invoke("set_auto_scan", { enabled: !autoScan.value });
    savedFlash.value = true;
    clearTimeout(savedTimer);
    savedTimer = setTimeout(() => (savedFlash.value = false), 1600);
  } catch (e) {
    scanError.value = errMsg(e);
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
  <div class="mx-auto max-w-3xl space-y-7 px-6 py-6">
    <!-- 媒体库：全局存储基础，不属于某个功能页 -->
    <SettingGroup label="媒体库">
      <SettingRow title="媒体库位置" truncate :desc="store.config?.libraryRoot || '未设置'">
        <!-- 路径不可编辑：给一个明确的"复制"出口，而不是让用户对着假输入框点 -->
        <button
          class="grid size-8 shrink-0 place-items-center rounded-lg border border-line text-ink-2 transition-colors hover:bg-hover disabled:opacity-45"
          :disabled="!store.config?.libraryRoot"
          :title="copied ? '已复制' : '复制路径'"
          @click="copyRoot"
        >
          <Check v-if="copied" class="size-4 text-ok" />
          <Copy v-else class="size-4" />
        </button>
        <button
          class="inline-flex h-8 shrink-0 items-center gap-1.5 rounded-lg bg-amber px-3 text-xs font-medium text-ink ring-1 ring-inset ring-amber-deep/45 transition-colors hover:bg-amber-deep disabled:opacity-55"
          :disabled="saving"
          @click="pickLibraryRoot"
        >
          <FolderPen class="size-3.5" />
          更改
        </button>
      </SettingRow>

      <SettingRow
        v-if="store.stats"
        title="库占用"
        :desc="`${store.stats.totalFiles.toLocaleString()} 个文件 · ${formatBytes(store.stats.totalBytes)}（与图库一致）`"
      >
        <span class="text-xs tabular-nums text-ink-2">剩余 {{ formatBytes(store.stats.freeBytes) }}</span>
      </SettingRow>

      <p v-if="error" class="flex items-center gap-1.5 bg-bad-soft px-5 py-2.5 text-xs text-bad">
        <TriangleAlert class="size-3.5 shrink-0" /> {{ error }}
      </p>
    </SettingGroup>

    <!-- 导入与归档：对应「拷贝整理」页的全部行为 -->
    <SettingGroup label="导入与归档">
      <SettingRow desc="检测到相机卡即生成归档计划；只读卡、不改卡，仍需手动点「开始拷贝」">
        <template #title>
          插卡后自动扫描
          <span v-if="savedFlash" class="text-[11px] font-normal text-ok">已保存</span>
        </template>
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
      </SettingRow>

      <!-- 静态说明不占常驻高度，折叠起来 -->
      <details class="group">
        <summary
          class="flex cursor-pointer list-none items-center gap-4 px-5 py-3.5 text-[13px] font-medium text-ink transition-colors hover:bg-hover/60"
        >
          <span class="flex-1">归档与去重规则</span>
          <ChevronDown class="chev size-4 text-ink-3 transition-transform duration-150" />
        </summary>
        <ul class="space-y-1.5 px-5 pb-4 text-xs leading-relaxed text-ink-2">
          <li>· 保持相机原厂文件名，不做任何改名</li>
          <li>· 照片 / RAW → <code class="rounded bg-subtle px-1">原图\年份\年-月\</code>；视频 → <code class="rounded bg-subtle px-1">视频\年份\年-月\</code></li>
          <li>· 无法确定拍摄时间的文件 → <code class="rounded bg-subtle px-1">原图\待定\</code></li>
          <li>· 同名同大小视为已存在、自动跳过；同名不同大小标为冲突，绝不覆盖</li>
          <li>· 拷贝阶段只新增文件，不改动或删除媒体库中已有内容</li>
        </ul>
      </details>

      <SettingRow title="清空导入记录" desc="每次拷贝结束自动记一条（含失败明细与取消），无卡首页展示最近 5 条">
        <button
          class="inline-flex h-8 shrink-0 items-center gap-1.5 rounded-lg border border-bad/40 px-3 text-xs text-bad transition-colors hover:bg-bad-soft disabled:opacity-55"
          :disabled="clearing"
          @click="clearLog"
        >
          清空记录
        </button>
      </SettingRow>

      <p v-if="scanError" class="flex items-center gap-1.5 bg-bad-soft px-5 py-2.5 text-xs text-bad">
        <TriangleAlert class="size-3.5 shrink-0" /> {{ scanError }}
      </p>
      <p v-if="clearedMsg" class="flex items-center gap-1.5 bg-ok-soft px-5 py-2.5 text-xs text-ok">
        <Check class="size-3.5 shrink-0" /> {{ clearedMsg }}
      </p>
      <p v-if="clearError" class="flex items-center gap-1.5 bg-bad-soft px-5 py-2.5 text-xs text-bad">
        <TriangleAlert class="size-3.5 shrink-0" /> {{ clearError }}
      </p>
    </SettingGroup>

    <!-- 图库：打开方式与评分互操作 -->
    <SettingGroup label="图库">
      <SettingRow truncate :desc="editorDesc">
        <template #title>
          外部编辑器
          <span v-if="editorSaved" class="text-[11px] font-normal text-ok">已保存</span>
        </template>
        <button
          class="inline-flex h-8 shrink-0 items-center gap-1.5 rounded-lg bg-amber px-3 text-xs font-medium text-ink ring-1 ring-inset ring-amber-deep/45 transition-colors hover:bg-amber-deep disabled:opacity-55"
          :disabled="editorBusy"
          @click="pickPhotoshop"
        >
          <FolderPen class="size-3.5" />
          选择
        </button>
        <button
          class="grid size-8 shrink-0 place-items-center rounded-lg border border-line text-ink-2 transition-colors hover:bg-hover disabled:opacity-45"
          :disabled="editorBusy || !configuredExe"
          title="恢复自动查找"
          aria-label="恢复自动查找"
          @click="savePhotoshopPath('')"
        >
          <RotateCcw class="size-4" />
        </button>
      </SettingRow>

      <SettingRow title="从照片文件导入旧评分" desc="读取 JPEG / AVIF·HEIC 内嵌 XMP，只补空缺、不覆盖已打的分">
        <button
          class="inline-flex h-8 shrink-0 items-center gap-1.5 rounded-lg border border-line px-3 text-xs text-ink-2 transition-colors hover:bg-hover disabled:opacity-45"
          :disabled="interopBusy"
          @click="importRatings"
        >
          导入
        </button>
      </SettingRow>

      <SettingRow title="导出评分到 XMP" desc="供 Bridge / Lightroom / darktable 读取；第三方 sidecar 一律跳过、绝不覆盖">
        <button
          class="inline-flex h-8 shrink-0 items-center gap-1.5 rounded-lg border border-line px-3 text-xs text-ink-2 transition-colors hover:bg-hover disabled:opacity-45"
          :disabled="interopBusy"
          @click="exportSidecars"
        >
          导出
        </button>
      </SettingRow>

      <SettingRow title="评分数据库" truncate :desc="dbPath || '—'">
        <button
          class="grid size-8 shrink-0 place-items-center rounded-lg border border-line text-ink-2 transition-colors hover:bg-hover disabled:opacity-45"
          :disabled="!dbPath"
          :title="dbCopied ? '已复制' : '复制数据库路径'"
          @click="copyDbPath"
        >
          <Check v-if="dbCopied" class="size-4 text-ok" />
          <Copy v-else class="size-4" />
        </button>
      </SettingRow>

      <p
        v-if="interopMsg"
        class="flex items-start gap-1.5 bg-ok-soft px-5 py-2.5 text-[11px] leading-relaxed text-ok"
      >
        <Check class="mt-0.5 size-3.5 shrink-0" /> {{ interopMsg }}
      </p>
      <p v-if="interopError" class="flex items-center gap-1.5 bg-bad-soft px-5 py-2.5 text-xs text-bad">
        <TriangleAlert class="size-3.5 shrink-0" /> {{ interopError }}
      </p>

      <p class="px-5 py-2.5 text-[11px] leading-relaxed text-ink-3">
        评分只存本地数据库、不写回照片文件；Windows 资源管理器不读 sidecar，其「分级」列不会显示。
        换电脑或重装前请备份上方数据库文件。
      </p>
    </SettingGroup>

    <!-- 智能相册：模型与在线地点服务 -->
    <SettingGroup label="智能相册">
      <ModelSettings />
      <PlaceSettings />
    </SettingGroup>
  </div>
</template>

<style scoped>
/* details 展开时箭头转向（Tailwind 的 group-open 在本项目版本下不够稳，直接写） */
details[open] .chev {
  transform: rotate(180deg);
}
</style>
