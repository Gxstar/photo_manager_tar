<script setup>
import { computed, ref } from "vue";
import { ChevronRight, FileImage, FileVideo, FileQuestion, Link2, TriangleAlert } from "lucide-vue-next";
import { formatBytes } from "../store.js";

const props = defineProps({
  files: { type: Array, required: true },
});

/** 展开的月份集合（默认全部折叠，只看汇总） */
const expanded = ref(new Set());
function toggle(month) {
  const s = new Set(expanded.value);
  s.has(month) ? s.delete(month) : s.add(month);
  expanded.value = s;
}

/** 按月份分组（后端已按 month_group 排序） */
const groups = computed(() => {
  const map = new Map();
  for (const f of props.files) {
    if (!map.has(f.monthGroup)) map.set(f.monthGroup, []);
    map.get(f.monthGroup).push(f);
  }
  return [...map.entries()].map(([month, files]) => {
    const newCount = files.filter((f) => f.status === "new").length;
    const existsCount = files.filter((f) => f.status === "exists").length;
    const conflictCount = files.filter((f) => f.status === "conflict").length;
    const copiedCount = files.filter((f) => f.status === "copied").length;
    const suspectCount = files.filter((f) => f.status === "suspectSkipped").length;
    const bytes = files.reduce((s, f) => s + f.size, 0);
    const newBytes = files
      .filter((f) => f.status === "new")
      .reduce((s, f) => s + f.size, 0);
    return {
      month,
      files,
      newCount,
      existsCount,
      conflictCount,
      copiedCount,
      suspectCount,
      bytes,
      newBytes,
    };
  });
});

const statusMeta = {
  new: { label: "新增", cls: "bg-amber-soft text-amber-text" },
  exists: { label: "已存在", cls: "bg-ok-soft text-ok" },
  conflict: { label: "冲突", cls: "bg-bad-soft text-bad" },
  suspectSkipped: { label: "改名跳过", cls: "bg-warn-soft text-warn" },
  copied: { label: "已拷贝", cls: "bg-ok-soft text-ok" },
  failed: { label: "失败", cls: "bg-bad-soft text-bad" },
};

const kindIcon = (f) =>
  f.kind === "video"
    ? FileVideo
    : f.kind === "sidecar"
      ? Link2
      : f.kind === "unknown"
        ? FileQuestion
        : FileImage;
</script>

<template>
  <div class="space-y-2">
    <section
      v-for="g in groups"
      :key="g.month"
      class="overflow-hidden rounded-card border bg-surface shadow-card"
      :class="g.conflictCount ? 'border-bad/40' : 'border-line'"
    >
      <!-- 月份汇总行（点击展开/收起） -->
      <button
        class="flex w-full items-center gap-2 px-4 py-2.5 text-left transition-colors hover:bg-hover/50"
        @click="toggle(g.month)"
      >
        <ChevronRight
          class="size-4 shrink-0 text-ink-3 transition-transform"
          :class="{ 'rotate-90': expanded.has(g.month) }"
        />
        <span class="text-sm font-semibold">{{ g.month }}</span>
        <span
          v-if="g.conflictCount"
          class="flex items-center gap-1 rounded-md bg-bad-soft px-1.5 py-0.5 text-[11px] font-medium text-bad"
        >
          <TriangleAlert class="size-3" /> 冲突 {{ g.conflictCount }}
        </span>

        <span class="ml-auto flex items-center gap-3 text-xs text-ink-2 tabular-nums">
          <span v-if="g.newCount" class="text-amber-text">新增 {{ g.newCount }} · {{ formatBytes(g.newBytes) }}</span>
          <span v-if="g.existsCount">已存在 {{ g.existsCount }}</span>
          <span v-if="g.suspectCount" class="text-warn">改名跳过 {{ g.suspectCount }}</span>
          <span v-if="g.copiedCount" class="text-ok">已拷 {{ g.copiedCount }}</span>
          <span class="text-ink-3">共 {{ g.files.length }} 个 · {{ formatBytes(g.bytes) }}</span>
        </span>
      </button>

      <!-- 明细（展开时） -->
      <div v-if="expanded.has(g.month)" class="border-t border-line">
        <div class="max-h-72 overflow-y-auto">
          <div
            v-for="f in g.files"
            :key="f.sourcePath"
            class="flex items-center gap-3 border-b border-line/60 px-4 py-1.5 text-[13px] last:border-b-0 hover:bg-hover/60"
          >
            <component :is="kindIcon(f)" class="size-4 shrink-0 text-ink-3" />
            <span class="min-w-0 flex-1 truncate" :title="f.fileName">{{ f.fileName }}</span>
            <span class="w-36 shrink-0 text-xs text-ink-2 tabular-nums">
              {{ f.shootTime || "—" }}
            </span>
            <span class="w-20 shrink-0 text-right text-xs text-ink-2 tabular-nums">
              {{ formatBytes(f.size) }}
            </span>
            <span
              class="w-16 shrink-0 rounded-md px-1.5 py-0.5 text-center text-[11px] font-medium"
              :class="statusMeta[f.status]?.cls"
            >
              {{ statusMeta[f.status]?.label ?? f.status }}
            </span>
            <span
              v-if="f.status === 'conflict' || f.note"
              class="w-44 shrink-0 truncate text-[11px] text-ink-3"
              :title="f.note || '库内同名文件字节数不同（可能为崩溃残留）'"
            >
              {{ f.note || "库内同名不同大小" }}
            </span>
            <span v-else class="w-44 shrink-0" />
          </div>
        </div>
      </div>
    </section>
  </div>
</template>
