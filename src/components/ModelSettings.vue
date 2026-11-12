<script setup>
import { computed, onMounted, ref } from "vue";
import { Channel, invoke } from "@tauri-apps/api/core";
import { ask } from "@tauri-apps/plugin-dialog";
import {
  Check,
  Copy,
  Download,
  FolderOpen,
  RefreshCw,
  Sparkles,
  Trash2,
  TriangleAlert,
  X,
} from "lucide-vue-next";
import SettingRow from "./SettingRow.vue";
import { errMsg } from "../store.js";

/**
 * 设置页「智能相册 · 模型」行（docs §38）。
 * 能力级选择：每张卡是一个能力，可独立下载 / 取消 / 删除，独立计费磁盘。
 * 状态分两级：模型文件是否齐全（ready）与索引是否可用（index），不把「下了」说成「能用」。
 * 下载只由用户点击触发，不自动、不后台（§17.15）；模型与索引只存本机。
 */
const models = ref([]);
const dbPath = ref("");
const loading = ref(true);
const error = ref("");
/** key → 是否正在下载 */
const busy = ref({});
/** key → 最近一条进度（DownloadProgress） */
const progress = ref({});
/** key → 下载 / 删除失败原因 */
const errors = ref({});
/** 语义索引任务（只有 cnclip 需要） */
const indexBusy = ref(false);
const indexKey = ref("");
const indexProgress = ref(null);
const indexError = ref("");
const indexPct = computed(() => {
  const p = indexProgress.value;
  if (!p || !p.total) return 0;
  return Math.min(100, Math.round((p.done / p.total) * 100));
});
// 路径行统一带复制出口（与「评分数据库」行一致）
const dbCopied = ref(false);
let dbCopiedTimer = 0;

async function copyDbPath() {
  if (!dbPath.value) return;
  try {
    await navigator.clipboard.writeText(dbPath.value);
    error.value = "";
    dbCopied.value = true;
    clearTimeout(dbCopiedTimer);
    dbCopiedTimer = setTimeout(() => (dbCopied.value = false), 1600);
  } catch {
    error.value = "复制失败，可直接选中路径文字手动复制";
  }
}

async function refresh(silent = false) {
  if (!silent) loading.value = true;
  error.value = "";
  try {
    const [m, d] = await Promise.all([invoke("smart_model_status"), invoke("smart_db_path")]);
    models.value = m || [];
    dbPath.value = d || "";
  } catch (e) {
    error.value = errMsg(e);
  } finally {
    if (!silent) loading.value = false;
  }
}

function isBusy(key) {
  return !!busy.value[key];
}

/** 整体进度：按「已完成文件数 + 当前文件比例」算，避免多文件时来回跳 0→100 */
function pct(key) {
  const p = progress.value[key];
  if (!p) return 0;
  const ratio = p.bytesTotal > 0 ? Math.min(1, p.bytesDone / p.bytesTotal) : p.done ? 1 : 0;
  return Math.min(100, Math.round(((p.fileIndex + ratio) / Math.max(1, p.fileTotal)) * 100));
}

/** 十进制 MB，与清单里的「759 MB」同口径 */
function fmtBytes(n) {
  if (!n) return "—";
  if (n >= 1e9) return `${(n / 1e9).toFixed(2)} GB`;
  return `${(n / 1e6).toFixed(1)} MB`;
}

function progressText(key) {
  const p = progress.value[key];
  if (!p) return "准备中…";
  return `${p.fileIndex + 1}/${p.fileTotal} · ${p.file} · ${fmtBytes(p.bytesDone)} / ${fmtBytes(p.bytesTotal)}`;
}

function badge(m) {
  if (isBusy(m.key)) return { text: `下载中 ${pct(m.key)}%`, cls: "bg-amber-soft text-amber" };
  if (!m.ready) return { text: "未下载", cls: "bg-subtle text-ink-2" };
  if (m.index?.state === "stale") return { text: "需重建索引", cls: "bg-amber-soft text-amber" };
  if (m.index?.state === "partial") return { text: "部分可用", cls: "bg-info-soft text-info" };
  if (m.index?.state === "complete") return { text: "可用", cls: "bg-ok-soft text-ok" };
  return { text: "已就绪", cls: "bg-ok-soft text-ok" };
}

function indexText(m) {
  if (m.key !== "cnclip") {
    return "模型已就绪，人脸检测与聚类将在下一批接入";
  }
  const idx = m.index;
  if (idx?.state === "stale") return "模型版本已变更，旧索引不能与新向量混算，需重建";
  if (idx?.state === "partial") return `已索引 ${idx.indexed} / ${idx.total} 张，搜索只覆盖已索引的部分`;
  if (idx?.state === "complete") return `全库已索引 ${idx.indexed} 张`;
  return "模型文件已就绪，尚未建立索引；建立后可中文搜图";
}

/** 后台全库索引：进度经 Channel 推送，可取消，已完成的照片保留。 */
async function buildIndex(key) {
  indexBusy.value = true;
  indexKey.value = key;
  indexProgress.value = { done: 0, total: 0, failed: 0, current: "扫描媒体库…" };
  indexError.value = "";
  const onProgress = new Channel();
  onProgress.onmessage = (p) => {
    indexProgress.value = p;
  };
  try {
    await invoke("smart_index_start", { onProgress });
    await refresh(true);
  } catch (e) {
    const msg = errMsg(e);
    if (!msg.includes("取消")) indexError.value = msg;
  } finally {
    indexBusy.value = false;
    indexKey.value = "";
    indexProgress.value = null;
  }
}

async function cancelIndex() {
  try {
    await invoke("smart_index_cancel");
  } catch (e) {
    indexError.value = errMsg(e);
  }
}

async function startDownload(key) {
  busy.value = { ...busy.value, [key]: true };
  errors.value = { ...errors.value, [key]: "" };
  progress.value = { ...progress.value, [key]: null };
  const onProgress = new Channel();
  onProgress.onmessage = (p) => {
    progress.value = { ...progress.value, [key]: p };
  };
  try {
    await invoke("smart_download_model", { key, onProgress });
    await refresh(true);
  } catch (e) {
    const msg = errMsg(e);
    // 用户主动取消不是错误，不污染错误条
    if (!msg.includes("取消")) errors.value = { ...errors.value, [key]: msg };
  } finally {
    busy.value = { ...busy.value, [key]: false };
    const next = { ...progress.value };
    delete next[key];
    progress.value = next;
  }
}

async function cancelDownload(key) {
  try {
    await invoke("smart_cancel_download", { key });
  } catch (e) {
    errors.value = { ...errors.value, [key]: errMsg(e) };
  }
}

async function removeModel(m) {
  const ok = await ask(
    `删除「${m.name}」的模型文件？将释放约 ${m.sizeMb} MB。已建立的索引会保留，照片文件不受影响；再次使用需要重新下载。`,
    { title: "删除模型", kind: "warning" },
  );
  if (!ok) return;
  errors.value = { ...errors.value, [m.key]: "" };
  try {
    await invoke("smart_delete_model", { key: m.key });
    await refresh(true);
  } catch (e) {
    errors.value = { ...errors.value, [m.key]: errMsg(e) };
  }
}

async function openModels() {
  try {
    await invoke("smart_open_models_dir");
  } catch (e) {
    error.value = errMsg(e);
  }
}

onMounted(refresh);
</script>

<template>
  <div v-if="loading" class="px-5 py-3.5 text-xs text-ink-2">检测模型文件…</div>

  <template v-else>
    <SettingRow v-for="m in models" :key="m.key">
      <template #title>
        {{ m.name }}
        <span
          class="inline-flex h-5 items-center gap-1 rounded-full px-1.5 text-[11px] font-normal"
          :class="badge(m).cls"
        >
          <Check v-if="m.ready && m.index?.state === 'complete'" class="size-3" />
          {{ badge(m).text }}
        </span>
      </template>

      <template #desc>
        {{ m.desc }}
        <span class="text-ink-3"> · {{ m.sizeMb }} MB · {{ m.license }}</span>
      </template>

      <template #below>
        <div v-if="isBusy(m.key)" class="mt-2">
          <div class="h-1.5 overflow-hidden rounded-full bg-subtle">
            <div
              class="h-full rounded-full bg-amber transition-[width] duration-200"
              :style="{ width: pct(m.key) + '%' }"
            />
          </div>
          <p class="mt-1 truncate text-[11px] text-ink-2" :title="progressText(m.key)">
            {{ progressText(m.key) }}
          </p>
        </div>

        <div v-else-if="m.ready" class="mt-1 space-y-1.5">
          <p
            class="text-[11px] leading-relaxed"
            :class="m.index?.state === 'stale' ? 'text-amber' : 'text-ink-2'"
          >
            {{ indexText(m) }}
          </p>
          <template v-if="m.key === 'cnclip'">
            <div v-if="indexBusy && indexKey === m.key" class="flex items-center gap-2">
              <div class="h-1.5 min-w-0 flex-1 overflow-hidden rounded-full bg-subtle">
                <div
                  class="h-full rounded-full bg-amber transition-[width] duration-200"
                  :style="{ width: indexPct + '%' }"
                />
              </div>
              <span class="shrink-0 text-[11px] tabular-nums text-ink-2">
                {{ indexProgress?.done || 0 }}/{{ indexProgress?.total || 0 }}
              </span>
              <button
                class="shrink-0 text-[11px] text-ink-2 underline transition-colors hover:text-ink"
                @click="cancelIndex"
              >
                取消
              </button>
            </div>
            <div v-else class="flex items-center gap-2">
              <span v-if="indexError" class="min-w-0 break-words text-[11px] text-bad">
                {{ indexError }}
              </span>
            </div>
          </template>
        </div>

        <p v-else class="mt-1 text-[11px] leading-relaxed text-ink-3">
          首次使用需下载 {{ m.sizeMb }} MB，点右侧「下载」即可；下载完成后自动校验
        </p>

        <p
          v-if="errors[m.key]"
          class="mt-1.5 flex items-start gap-1.5 rounded-md bg-bad-soft px-2.5 py-1.5 text-[11px] leading-4 text-bad"
        >
          <TriangleAlert class="mt-0.5 size-3.5 shrink-0" />
          <span class="min-w-0 break-words">{{ errors[m.key] }}</span>
        </p>
      </template>

      <!-- 已就绪的语义搜图：把「建立索引」放在卡片右侧主按钮位，避免找不到 -->
      <template v-if="m.ready && m.key === 'cnclip'">
        <button
          v-if="indexBusy && indexKey === m.key"
          class="inline-flex h-8 shrink-0 items-center gap-1.5 rounded-lg border border-line px-2.5 text-xs text-ink-2 transition-colors hover:bg-hover"
          @click="cancelIndex"
        >
          索引中…
        </button>
        <button
          v-else
          class="inline-flex h-8 shrink-0 items-center gap-1.5 rounded-lg bg-amber px-3 text-xs font-medium text-ink ring-1 ring-inset ring-amber-deep/45 transition-colors hover:bg-amber-deep"
          @click="buildIndex(m.key)"
        >
          <Sparkles class="size-3.5" />
          {{ m.index?.state === "empty" ? "建立索引" : "重建索引" }}
        </button>
        <button
          class="grid size-8 shrink-0 place-items-center rounded-lg border border-line text-ink-2 transition-colors hover:bg-hover"
          title="删除模型文件（索引保留）"
          @click="removeModel(m)"
        >
          <Trash2 class="size-3.5" />
        </button>
      </template>
      <button
        v-else-if="isBusy(m.key)"
        class="inline-flex h-8 shrink-0 items-center gap-1.5 rounded-lg border border-line px-2.5 text-xs text-ink-2 transition-colors hover:bg-hover"
        @click="cancelDownload(m.key)"
      >
        <X class="size-3.5" /> 取消
      </button>
      <button
        v-else-if="!m.ready"
        class="inline-flex h-8 shrink-0 items-center gap-1.5 rounded-lg bg-amber px-3 text-xs font-medium text-ink ring-1 ring-inset ring-amber-deep/45 transition-colors hover:bg-amber-deep"
        @click="startDownload(m.key)"
      >
        <Download class="size-3.5" /> 下载
      </button>
      <button
        v-else
        class="inline-flex h-8 shrink-0 items-center gap-1.5 rounded-lg border border-line px-2.5 text-xs text-ink-2 transition-colors hover:bg-hover"
        @click="removeModel(m)"
      >
        <Trash2 class="size-3.5" /> 删除
      </button>
    </SettingRow>

    <SettingRow title="智能相册索引" truncate :desc="dbPath || '—'">
      <button
        class="grid size-8 shrink-0 place-items-center rounded-lg border border-line text-ink-2 transition-colors hover:bg-hover disabled:opacity-45"
        :disabled="!dbPath"
        :title="dbCopied ? '已复制' : '复制索引路径'"
        @click="copyDbPath"
      >
        <Check v-if="dbCopied" class="size-4 text-ok" />
        <Copy v-else class="size-4" />
      </button>
    </SettingRow>
  </template>

  <p v-if="error" class="flex items-center gap-1.5 bg-bad-soft px-5 py-2.5 text-xs text-bad">
    <TriangleAlert class="size-3.5 shrink-0" /> {{ error }}
  </p>

  <!-- 模型块的收尾行：不做底色块，避免把卡片切断成两段 -->
  <div class="flex flex-wrap items-center justify-between gap-2 px-5 py-2.5">
    <span class="text-[11px] leading-relaxed text-ink-3">
      模型只在你点击下载时联网；不自动、不后台。模型与索引只存本机，不上传任何照片或文件信息
    </span>
    <div class="flex gap-2">
      <button
        class="inline-flex h-8 shrink-0 items-center gap-1.5 rounded-lg border border-line px-2.5 text-xs text-ink-2 transition-colors hover:bg-hover"
        @click="openModels"
      >
        <FolderOpen class="size-3.5" /> 打开模型目录
      </button>
      <button
        class="inline-flex h-8 shrink-0 items-center gap-1.5 rounded-lg border border-line px-2.5 text-xs text-ink-2 transition-colors hover:bg-hover"
        @click="refresh(false)"
      >
        <RefreshCw class="size-3.5" /> 重新检测
      </button>
    </div>
  </div>
</template>
