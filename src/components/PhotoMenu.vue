<script setup>
import { computed } from "vue";

/**
 * 通用照片右键菜单（docs §33）。
 *
 * 图库与智能相册共用：只负责「按 items 渲染 + 定位」，动作与状态留在页面里。
 * Teleport 到 body + fixed：避免被 overflow 容器 / 虚拟行 transform 裁切。
 * `data-ctx-menu` 供页面判断「点击是否落在菜单内」，不要改。
 */
const props = defineProps({
  open: { type: Boolean, default: false },
  pos: { type: Object, default: () => ({ top: 0, left: 0 }) },
  /** 顶部标题栏，通常是文件名或「已选 N 项」 */
  title: { type: String, default: "" },
  width: { type: Number, default: 200 },
  /** [{ key, label, icon, hint?, disabled?, separatorBefore? }] */
  items: { type: Array, default: () => [] },
});
const emit = defineEmits(["pick"]);

const style = computed(() => ({
  top: `${props.pos.top}px`,
  left: `${props.pos.left}px`,
  width: `${props.width}px`,
}));
</script>

<template>
  <Teleport to="body">
    <div
      v-if="open"
      class="fixed z-50 rounded-card border border-line bg-surface p-1 shadow-card"
      :style="style"
      data-ctx-menu
      @contextmenu.prevent
    >
      <div class="truncate px-2.5 py-1 text-[11px] text-ink-3" :title="title">{{ title }}</div>
      <div class="my-1 h-px bg-line"></div>
      <template v-for="(it, i) in items" :key="it.key">
        <div v-if="it.separatorBefore && i > 0" class="my-1 h-px bg-line"></div>
        <button
          type="button"
          class="flex w-full items-center gap-2 rounded-lg px-2.5 py-1.5 text-left text-xs text-ink-2 transition-colors"
          :class="it.disabled ? 'cursor-not-allowed opacity-45' : 'hover:bg-hover'"
          :disabled="it.disabled"
          @click="emit('pick', it.key)"
        >
          <component :is="it.icon" class="size-3.5 shrink-0" />
          <span class="min-w-0 truncate">{{ it.label }}</span>
          <span v-if="it.hint" class="ml-auto shrink-0 text-[10px] text-ink-3">{{ it.hint }}</span>
        </button>
      </template>
    </div>
  </Teleport>
</template>
