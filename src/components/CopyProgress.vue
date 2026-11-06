<script setup>
import { computed } from "vue";
import { X } from "lucide-vue-next";
import { formatBytes, formatEta } from "../store.js";

const props = defineProps({
  progress: { type: Object, required: true },
});
const emit = defineEmits(["cancel"]);

const pct = computed(() => {
  if (!props.progress.totalBytes) return 0;
  return Math.min(100, (props.progress.doneBytes / props.progress.totalBytes) * 100);
});
</script>

<template>
  <div class="rounded-card border border-line bg-surface p-4 shadow-card">
    <div class="mb-2 flex items-baseline justify-between">
      <span class="flex items-baseline gap-2 text-sm font-medium">
        <span>正在拷贝 {{ progress.doneFiles }} / {{ progress.totalFiles }} 个文件</span>
        <span class="text-xs font-normal text-ink-2">
          {{ formatBytes(progress.doneBytes) }} / {{ formatBytes(progress.totalBytes) }}
        </span>
        <span class="text-xs font-normal text-ink-2 tabular-nums">{{ Math.round(pct) }}%</span>
      </span>
      <button
        class="flex items-center gap-1 rounded-lg border border-line px-2.5 py-1 text-xs text-ink-2 transition-colors hover:border-bad hover:text-bad"
        @click="emit('cancel')"
      >
        <X class="size-3.5" /> 取消
      </button>
    </div>

    <div class="h-2 w-full overflow-hidden rounded-full bg-subtle">
      <div
        class="h-full rounded-full bg-amber transition-[width] duration-150"
        :style="{ width: pct + '%' }"
      />
    </div>

    <div class="mt-2 flex items-center justify-between text-xs text-ink-2">
      <span class="min-w-0 flex-1 truncate">
        {{ progress.current }}
      </span>
      <span class="ml-4 shrink-0 tabular-nums">
        {{ formatBytes(progress.bps) }}/s · 剩余约 {{ formatEta(progress.eta) }}
      </span>
    </div>
  </div>
</template>
