<script setup>
import { computed, ref, watch } from "vue";
import { ImageOff } from "lucide-vue-next";

/** 相册封面卡（docs §19.4）：统一 4:3、object-fit cover、张数角标、无图降级。 */
const props = defineProps({
  title: { type: String, default: "" },
  subtitle: { type: String, default: "" },
  count: { type: Number, default: 0 },
  coverPath: { type: String, default: "" },
  selected: { type: Boolean, default: false },
});
const emit = defineEmits(["select"]);
const broken = ref(false);
const coverUrl = computed(() =>
  props.coverPath ? `http://thumb.localhost/${encodeURIComponent(props.coverPath)}?v=2` : "",
);
watch(() => props.coverPath, () => (broken.value = false));
</script>

<template>
  <button
    type="button"
    class="group block w-full overflow-hidden rounded-card border bg-surface text-left shadow-card transition-colors"
    :class="selected ? 'border-amber' : 'border-line hover:border-line-strong'"
    @click="emit('select')"
  >
    <div class="relative aspect-[4/3] bg-subtle">
      <img
        v-if="coverUrl && !broken"
        :src="coverUrl"
        class="h-full w-full object-cover"
        alt=""
        draggable="false"
        @error="broken = true"
      />
      <div v-else class="grid h-full place-items-center text-ink-3">
        <div class="text-center text-xs">
          <ImageOff class="mx-auto mb-1 size-5" />
          <span>无可用缩略图</span>
        </div>
      </div>
      <span
        class="absolute bottom-1.5 right-1.5 rounded-md px-1.5 py-0.5 text-[11px] text-white"
        style="background: rgba(30, 26, 22, 0.92)"
      >{{ count }} 张</span>
    </div>
    <div class="p-2.5" :class="selected ? 'bg-amber-soft' : ''">
      <div class="truncate text-xs font-medium" :title="title">{{ title }}</div>
      <div class="mt-0.5 truncate text-[11px] text-ink-2">{{ subtitle }}</div>
    </div>
  </button>
</template>
