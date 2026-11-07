<script setup>
import { onMounted, ref } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { Check, Copy, FolderOpen, RefreshCw, TriangleAlert } from "lucide-vue-next";
import SettingRow from "./SettingRow.vue";

/** 设置页「智能相册 · 模型」行（docs §17.15 / §19.2）。状态全部来自真实目录检测。 */
const models = ref([]);
const dbPath = ref("");
const loading = ref(true);
const error = ref("");
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

async function refresh() {
  loading.value = true;
  error.value = "";
  try {
    const [m, d] = await Promise.all([invoke("smart_model_status"), invoke("smart_db_path")]);
    models.value = m || [];
    dbPath.value = d || "";
  } catch (e) {
    error.value = String(e);
  } finally {
    loading.value = false;
  }
}

async function openModels() {
  try {
    await invoke("smart_open_models_dir");
  } catch (e) {
    error.value = String(e);
  }
}

onMounted(refresh);
</script>

<template>
  <div v-if="loading" class="px-5 py-3.5 text-xs text-ink-2">检测模型文件…</div>

  <template v-else>
  <SettingRow
    v-for="m in models"
    :key="m.key"
    :desc="`${m.sizeMb} MB · ${m.license}${!m.ready && m.missing.length ? ` · 缺 ${m.missing.length} 个文件` : ''}${!m.ready ? ' · 内置下载待接入' : ''}`"
  >
    <template #title>
      {{ m.name }}
      <span
        class="inline-flex h-5 items-center gap-1 rounded-full px-1.5 text-[11px] font-normal"
        :class="m.ready ? 'bg-ok-soft text-ok' : 'bg-subtle text-ink-2'"
      >
        <Check v-if="m.ready" class="size-3" />
        {{ m.ready ? "已就绪" : "未下载" }}
      </span>
    </template>
    <span v-if="m.ready && m.source" class="text-[11px] text-info">{{ m.source }}</span>
  </SettingRow>
  </template>

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

  <p v-if="error" class="flex items-center gap-1.5 bg-bad-soft px-5 py-2.5 text-xs text-bad">
    <TriangleAlert class="size-3.5 shrink-0" /> {{ error }}
  </p>

  <!-- 模型块的收尾行：不做底色块，避免把卡片切断成两段 -->
  <div class="flex flex-wrap items-center justify-between gap-2 px-5 py-2.5">
    <span class="text-[11px] leading-relaxed text-ink-3">
      模型与索引只存本机，不上传任何照片或文件信息
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
        @click="refresh"
      >
        <RefreshCw class="size-3.5" /> 重新检测
      </button>
    </div>
  </div>
</template>
