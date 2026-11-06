<script setup>
import { computed, onBeforeUnmount, ref, watch } from "vue";
import { invoke, Channel } from "@tauri-apps/api/core";
import { FolderSearch, RefreshCw, LoaderCircle, Play, CircleCheck, TriangleAlert, RotateCw } from "lucide-vue-next";
import DeviceCard from "../components/DeviceCard.vue";
import PlanTable from "../components/PlanTable.vue";
import CopyProgress from "../components/CopyProgress.vue";
import HomeEmpty from "../components/HomeEmpty.vue";
import {
  store,
  refreshDevices,
  resetHome,
  errMsg,
  formatBytes,
} from "../store.js";

const scanError = ref(null);
const copyingError = ref(null);

const hasCard = computed(() => store.devices.some((d) => d.hasMedia));
const selected = computed(
  () => store.devices.find((d) => d.path === store.selectedDevice) ?? null
);

const deficit = computed(() => {
  if (!store.plan || store.plan.spaceOk) return null;
  return store.plan.newBytes - store.plan.freeBytes;
});

let pollTimer = null;
// 轮询设备列表：插卡 ≤3s 出现；扫描/拷贝中不打扰
pollTimer = setInterval(() => {
  if (!store.copying && !store.scanning) refreshDevices();
}, 3000);
onBeforeUnmount(() => clearInterval(pollTimer));

// ── 插卡自动扫描 ─────────────────────────────────────────────────────────
// 设备轮询检测到「有卡的设备」后自动选中并扫描一次，省掉每天重复的一次点击。
// 扫描只读卡（不做任何写入），扫描结果只是计划表，仍需手动点「开始拷贝」。
// 标记存 store 而非组件内 ref：切页回来组件重建时不重复扫描。
watch(
  () => store.devices,
  (list) => {
    const card = list.find((d) => d.hasMedia);
    if (!card) {
      // 拔卡：重置自动扫描标记，旧计划随之失效（下次插卡重新扫描）
      store.autoScannedDevice = null;
      if (!store.copying && !store.scanning) {
        store.plan = null;
        store.report = null;
      }
      return;
    }
    if (!store.selectedDevice) store.selectedDevice = card.path;
    if (store.selectedDevice !== card.path) return;
    // 已有该卡的计划：不再自动扫（避免切页回来重扫、覆盖已拷报告）
    if (store.plan?.devicePath === card.path) {
      store.autoScannedDevice = card.path;
      return;
    }
    if (store.config?.autoScan === false) return;
    if (store.autoScannedDevice === card.path) return;
    if (store.scanning || store.copying) return;
    store.autoScannedDevice = card.path;
    pickAndScan();
  },
  { immediate: true }
);

/** 异步扫描：立即返回，进度经 Channel 推送 */
function pickAndScan() {
  if (!selected.value || !selected.value.hasMedia || store.scanning) return;
  store.scanning = true;
  store.scanProgress = null;
  store.plan = null;
  store.report = null;
  scanError.value = null;
  copyingError.value = null;

  const onEvent = new Channel();
  onEvent.onmessage = (msg) => {
    switch (msg.event) {
      case "progress":
        store.scanProgress = {
          done: msg.data.done,
          total: msg.data.total,
          current: msg.data.currentFile,
        };
        break;
      case "finished":
        store.plan = msg.data.plan;
        store.scanning = false;
        store.scanProgress = null;
        break;
      case "failed":
        scanError.value = msg.data.message;
        store.scanning = false;
        store.scanProgress = null;
        break;
    }
  };
  invoke("scan_card", { devicePath: selected.value.path, onEvent }).catch((e) => {
    scanError.value = errMsg(e);
    store.scanning = false;
    store.scanProgress = null;
  });
}

function onCopyEvent(msg) {
  switch (msg.event) {
    case "started":
      store.progress = {
        doneFiles: 0,
        doneBytes: 0,
        totalFiles: msg.data.totalFiles,
        totalBytes: msg.data.totalBytes,
        current: "",
        bps: 0,
        eta: null,
      };
      break;
    case "progress":
      if (store.progress) {
        store.progress.doneFiles = msg.data.doneFiles;
        store.progress.doneBytes = msg.data.doneBytes;
        store.progress.current = msg.data.currentFile;
        store.progress.bps = msg.data.bytesPerSec;
        store.progress.eta = msg.data.etaSecs;
      }
      break;
    case "fileFailed": {
      const f = store.plan?.files.find((x) => x.fileName === msg.data.fileName);
      if (f) {
        f.status = "failed";
        f.note = msg.data.reason;
      }
      break;
    }
    case "finished":
      store.copying = false;
      store.progress = null;
      store.report = {
        copied: msg.data.copied,
        failed: msg.data.failed,
        elapsed: msg.data.elapsedSecs,
        avgBps: msg.data.avgBytesPerSec,
      };
      // 拷贝完成后本地刷新三态（已拷的标已拷贝）
      for (const f of store.plan?.files ?? []) {
        if (f.status === "new") f.status = "copied";
      }
      // 库内容变了：下次进入无卡空态必须重取（拔卡后展示刚导入的照片）
      resetHome();
      break;
    case "cancelled":
      store.copying = false;
      store.progress = null;
      // 取消后重扫一次拿准确三态
      if (store.selectedDevice) pickAndScan();
      break;
  }
}

function startCopy() {
  if (!store.plan) return;
  copyingError.value = null;
  store.copying = true;
  store.report = null;
  const onEvent = new Channel();
  onEvent.onmessage = onCopyEvent;
  invoke("start_copy", { plan: store.plan, onEvent }).catch((e) => {
    store.copying = false;
    copyingError.value = errMsg(e);
  });
}

async function cancelCopy() {
  try {
    await invoke("cancel_copy");
  } catch (e) {
    copyingError.value = errMsg(e);
  }
}
</script>

<template>
  <div class="flex min-h-full flex-col gap-4 p-6">
    <!-- 无卡：首页空态（最近导入 + 归档概况 + 入口） -->
    <HomeEmpty v-if="!hasCard" @refresh="refreshDevices" />

    <template v-else>
      <!-- 设备区 -->
      <section>
        <div class="mb-2.5 flex items-center justify-between">
          <h2 class="text-sm font-semibold text-ink-2">设备</h2>
          <button
            class="flex items-center gap-1.5 rounded-lg px-2.5 py-1 text-xs text-ink-2 transition-colors hover:bg-hover"
            :disabled="store.devicesLoading || store.copying || store.scanning"
            @click="refreshDevices"
          >
            <RefreshCw class="size-3.5" :class="{ 'animate-spin': store.devicesLoading }" />
            刷新
          </button>
        </div>

        <div class="flex flex-wrap gap-3">
          <DeviceCard
            v-for="d in store.devices"
            :key="d.path"
            :device="d"
            :selected="store.selectedDevice === d.path"
            :disabled="store.copying || store.scanning"
            @select="store.selectedDevice = d.hasMedia ? d.path : store.selectedDevice"
          />
        </div>
      </section>

      <!-- 扫描 -->
      <section v-if="selected?.hasMedia" class="flex items-center gap-3">
        <button
          class="flex h-9 items-center gap-2 rounded-lg bg-amber px-4 text-sm font-medium text-ink ring-1 ring-inset ring-amber-deep/45 shadow-sm transition-colors hover:bg-amber-deep disabled:cursor-not-allowed disabled:opacity-55"
          :disabled="store.scanning || store.copying"
          @click="pickAndScan"
        >
          <LoaderCircle v-if="store.scanning" class="size-4 animate-spin" />
          <FolderSearch v-else class="size-4" />
          {{ store.scanning ? "扫描中…" : "扫描卡片" }}
        </button>
        <span v-if="scanError" class="flex items-center gap-1.5 text-xs text-bad">
          <TriangleAlert class="size-3.5" /> {{ scanError }}
        </span>
      </section>

      <!-- 扫描进度 -->
      <section
        v-if="store.scanning && store.scanProgress"
        class="rounded-card border border-line bg-surface p-4 shadow-card"
      >
        <div class="mb-2 flex items-baseline justify-between text-sm">
          <span class="font-medium">
            正在扫描 {{ store.scanProgress.done }} / {{ store.scanProgress.total }} 个文件
          </span>
          <span class="text-xs text-ink-3">
            {{ Math.round((store.scanProgress.done / Math.max(1, store.scanProgress.total)) * 100) }}%
          </span>
        </div>
        <div class="h-2 w-full overflow-hidden rounded-full bg-subtle">
          <div
            class="h-full rounded-full bg-amber transition-[width] duration-100"
            :style="{
              width:
                Math.min(100, (store.scanProgress.done / Math.max(1, store.scanProgress.total)) * 100) + '%',
            }"
          />
        </div>
        <div class="mt-2 truncate text-xs text-ink-3">
          {{ store.scanProgress.current }}
        </div>
      </section>

      <!-- 计划预览（月份折叠卡片） -->
      <template v-if="store.plan">
        <section class="flex items-center gap-2">
          <span class="text-sm font-semibold text-ink-2">归档计划</span>
          <span class="text-xs text-ink-3">点击月份可展开明细</span>
        </section>

        <PlanTable :files="store.plan.files" />

        <!-- 底部操作条 / 进度 -->
        <div class="sticky bottom-0 -mx-6 mt-2 border-t border-line bg-app/95 px-6 pb-4 pt-3 backdrop-blur">
          <CopyProgress
            v-if="store.copying && store.progress"
            :progress="store.progress"
            @cancel="cancelCopy"
          />

          <template v-else>
            <!-- 完成汇总 -->
            <div
              v-if="store.report"
              class="mb-3 flex items-center gap-4 rounded-card border border-line bg-surface px-4 py-2.5 text-sm shadow-card"
            >
              <span class="flex items-center gap-1.5 text-ok">
                <CircleCheck class="size-4" /> 拷贝完成
              </span>
              <span class="text-ink-2">
                成功 {{ store.report.copied }} 个
                <template v-if="store.report.failed"> · 失败 {{ store.report.failed }} 个</template>
                · 用时 {{ store.report.elapsed }} 秒 · 平均 {{ formatBytes(store.report.avgBps) }}/s
              </span>
              <button
                class="ml-auto flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1 text-xs text-ink-2 hover:bg-hover"
                @click="store.selectedDevice && pickAndScan()"
              >
                <RotateCw class="size-3.5" /> 再扫一次核对
              </button>
            </div>

            <div v-if="copyingError" class="mb-2 text-xs text-bad">{{ copyingError }}</div>

            <div class="flex items-center gap-4">
              <span class="text-sm text-ink-2">
                待拷
                <b class="text-ink">{{ store.plan.newFiles }}</b> 个文件 ·
                {{ formatBytes(store.plan.newBytes) }}
                <template v-if="store.plan.existsFiles">
                  · 跳过已存在 {{ store.plan.existsFiles }} 个
                </template>
                <template v-if="store.plan.suspectFiles">
                  · <span class="text-warn">改名跳过 {{ store.plan.suspectFiles }} 个</span>
                </template>
                <template v-if="store.plan.conflictFiles">
                  · <span class="text-bad">冲突 {{ store.plan.conflictFiles }} 个</span>
                </template>
              </span>
              <span v-if="deficit" class="text-xs text-bad">
                目标盘剩余 {{ formatBytes(store.plan.freeBytes) }}，还差 {{ formatBytes(deficit) }}
              </span>
              <button
                class="ml-auto h-10 rounded-xl bg-amber px-6 text-sm font-semibold text-ink ring-1 ring-inset ring-amber-deep/45 shadow-sm transition-colors hover:bg-amber-deep disabled:cursor-not-allowed disabled:opacity-55"
                :disabled="!store.plan.spaceOk || store.plan.newFiles === 0"
                @click="startCopy"
              >
                <Play class="mr-1.5 inline size-4 -translate-y-px" />
                开始拷贝
              </button>
            </div>
          </template>
        </div>
      </template>
    </template>
  </div>
</template>
