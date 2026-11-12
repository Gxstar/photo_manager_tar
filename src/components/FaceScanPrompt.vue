<script setup>
/**
 * 首次启动的人脸扫描知情同意（docs §38 人物批）。
 *
 * 人脸属生物特征，**必须先问再扫**：用户点「允许」才写入 `face_auto_scan=on` 并后台扫描；
 * 点「暂不」写入 `off`，从此只有手动点「扫描人脸」才会扫。
 * 文案只承诺真实行为：本地处理、不改照片、可随时取消、可一键删除。
 */
import { ScanSearch, ShieldCheck, X } from "lucide-vue-next";

defineProps({
  open: { type: Boolean, default: false },
});
const emit = defineEmits(["choose"]);

const points = [
  { icon: ShieldCheck, text: "全程在本机处理，不上传任何照片或人脸数据" },
  { icon: ScanSearch, text: "只读取缩略图，不改动、不移动你的照片文件" },
];
</script>

<template>
  <Teleport to="body">
    <div
      v-if="open"
      class="fixed inset-0 z-50 grid place-items-center px-4"
      style="background-color: rgb(46 42 36 / 0.30)"
    >
      <div
        class="w-full max-w-md overflow-hidden rounded-card border border-line bg-surface shadow-card"
        role="dialog"
        aria-modal="true"
        aria-labelledby="face-prompt-title"
      >
        <header class="flex items-start gap-3 border-b border-line px-5 py-4">
          <span class="grid size-9 shrink-0 place-items-center rounded-xl bg-amber-soft text-amber">
            <ScanSearch class="size-4.5" />
          </span>
          <div class="min-w-0 flex-1">
            <h2 id="face-prompt-title" class="text-sm font-semibold">自动识别照片里的人</h2>
            <p class="mt-0.5 text-[11px] leading-4 text-ink-2">
              首次使用前先征求你的同意
            </p>
          </div>
          <button
            class="grid size-7 shrink-0 place-items-center rounded-lg text-ink-2 transition-colors hover:bg-hover"
            aria-label="关闭"
            @click="emit('choose', 'off')"
          >
            <X class="size-4" />
          </button>
        </header>

        <div class="px-5 py-4">
          <p class="text-xs leading-relaxed text-ink-2">
            开启后，「显影」会在启动约 20 秒后用后台低优先级线程扫描媒体库，
            把同一个人的照片聚成一组，等你命名成人物册；扫完之前也能随时叫停。
          </p>

          <ul class="mt-3 space-y-1.5">
            <li v-for="p in points" :key="p.text" class="flex items-start gap-2 text-xs text-ink-2">
              <component :is="p.icon" class="mt-0.5 size-3.5 shrink-0 text-ok" />
              <span class="leading-relaxed">{{ p.text }}</span>
            </li>
          </ul>

          <p class="mt-3 rounded-lg bg-subtle px-3 py-2 text-[11px] leading-4 text-ink-3">
            人脸属于生物特征数据。选「暂不扫描」也可以随时在「智能相册 → 人物」里手动开始；
            自动扫描开关同样能在那里随时关掉，已扫数据可一键清除。
          </p>
        </div>

        <footer class="flex items-center justify-end gap-2 border-t border-line px-5 py-3">
          <button
            class="flex h-8 items-center rounded-lg border border-line px-3 text-xs text-ink-2 transition-colors hover:bg-hover"
            @click="emit('choose', 'off')"
          >暂不扫描</button>
          <button
            class="flex h-8 items-center gap-1.5 rounded-lg bg-amber px-3.5 text-xs font-medium text-ink ring-1 ring-inset ring-amber-deep/45 transition-colors hover:bg-amber-deep"
            @click="emit('choose', 'on')"
          >允许后台自动扫描</button>
        </footer>
      </div>
    </div>
  </Teleport>
</template>
