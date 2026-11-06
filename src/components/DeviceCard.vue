<script setup>
import { computed } from "vue";
import { RefreshCw, HardDrive, Camera, CircleAlert } from "lucide-vue-next";
import { formatBytes } from "../store.js";

const props = defineProps({
  device: { type: Object, required: true },
  selected: { type: Boolean, default: false },
  disabled: { type: Boolean, default: false },
});
const emit = defineEmits(["select"]);

// 空读卡器的 label 就是盘符（I:），再显示一遍路径（I:\）只是重复
const showPath = computed(() => {
  const label = (props.device.label ?? "").trim();
  const path = props.device.path.replace(/\\+$/, "");
  return !label || label !== path;
});
</script>

<template>
  <button
    class="flex w-64 flex-col items-start gap-2 rounded-card border bg-surface p-4 text-left shadow-card transition-all disabled:cursor-not-allowed disabled:opacity-55"
    :class="selected ? 'border-amber ring-1 ring-amber' : 'border-line hover:border-line-strong'"
    :disabled="disabled"
    @click="emit('select')"
  >
    <div class="flex w-full items-center gap-2.5">
      <span
        class="grid size-9 shrink-0 place-items-center rounded-lg"
        :class="device.hasMedia ? 'bg-amber-soft text-amber' : 'bg-subtle text-ink-3'"
      >
        <Camera v-if="device.isCameraCard" class="size-4.5" />
        <HardDrive v-else class="size-4.5" />
      </span>
      <div class="min-w-0 leading-tight">
        <div class="truncate text-sm font-medium">
          {{ device.label || device.path.replace('\\', '') }}
        </div>
        <div class="text-xs text-ink-3" v-if="showPath">{{ device.path }}</div>
      </div>
    </div>

    <!-- 空读卡器：单独状态，不报错 -->
    <div v-if="!device.hasMedia" class="text-xs text-ink-3">读卡器（无卡）</div>

    <template v-else>
      <div class="flex flex-wrap gap-1.5">
        <span
          v-if="device.isCameraCard"
          class="rounded-md bg-amber-soft px-1.5 py-0.5 text-[11px] font-medium text-amber-text"
        >
          相机卡{{ device.cardKind ? ` · ${device.cardKind}` : '' }}
        </span>
        <span v-else class="rounded-md bg-info-soft px-1.5 py-0.5 text-[11px] text-info">
          可移动卷
        </span>
      </div>
      <div class="w-full">
        <div class="h-1.5 w-full overflow-hidden rounded-full bg-subtle">
          <div
            class="h-full rounded-full bg-amber"
            :style="{ width: Math.min(100, (1 - device.freeBytes / device.totalBytes) * 100) + '%' }"
          />
        </div>
        <div class="mt-1 flex justify-between text-[11px] text-ink-3">
          <span>{{ formatBytes(device.totalBytes - device.freeBytes) }} 已用</span>
          <span>剩 {{ formatBytes(device.freeBytes) }}</span>
        </div>
      </div>
    </template>
  </button>
</template>
