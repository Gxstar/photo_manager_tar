<script setup>
/**
 * 设置页「一行一项」的通用行（docs §34）
 * 结构固定：左侧标题 + 一行描述，右侧控件右对齐；分隔线由 SettingGroup 的 divide-y 提供。
 * 描述只有一行时传 `truncate`（路径类），多行说明不要传。
 */
defineProps({
  title: { type: String, default: "" },
  desc: { type: String, default: "" },
  truncate: { type: Boolean, default: false },
});
</script>

<template>
  <div class="flex items-center gap-4 px-5 py-3.5">
    <div class="min-w-0 flex-1">
      <div class="flex items-center gap-2 text-[13px] font-medium leading-5 text-ink">
        <slot name="title">{{ title }}</slot>
      </div>
      <p
        v-if="desc || $slots.desc"
        class="mt-0.5 select-text text-xs leading-relaxed text-ink-2"
        :class="truncate && 'truncate'"
      >
        <slot name="desc">{{ desc }}</slot>
      </p>
      <slot name="below" />
    </div>
    <div class="flex shrink-0 items-center gap-2">
      <slot />
    </div>
  </div>
</template>
