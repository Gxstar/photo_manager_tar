<script setup>
/**
 * 设置页分区容器（视觉规范见 docs §34）
 * 布局约定：分组标签在卡片外（小号灰字），卡片内所有行由 `divide-y` 自动分隔，
 * 因此每个 SettingRow / details / 底部 meta 条都不需要自己画分隔线。
 */
defineProps({
  label: { type: String, required: true },
});
</script>

<template>
  <section>
    <h2 class="mb-2 px-1 text-xs font-medium tracking-[0.06em] text-ink-2">
      {{ label }}
    </h2>
    <div class="divide-y divide-line overflow-hidden rounded-card border border-line bg-surface shadow-card">
      <slot />
    </div>
    <slot name="note" />
  </section>
</template>
