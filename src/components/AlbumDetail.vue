<script setup>
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { invoke } from "@tauri-apps/api/core";
import {
  ArrowLeft,
  Aperture,
  Pencil,
  Check,
  X,
  FileImage,
  ImageOff,
  Star,
  ExternalLink,
  WandSparkles,
  FolderOpen,
  RefreshCw,
  TriangleAlert,
} from "lucide-vue-next";
import PhotoMenu from "./PhotoMenu.vue";

/**
 * 通用「相册详情」视图（docs §30）。
 *
 * **一致性约定：人物 / 宠物 / 地点三类相册点进去都必须走这个组件**，
 * 不允许在列表页里内联铺图。宿主只负责提供照片路径与动作回调，
 * 布局、选中、信息条、封面、键盘、空态全部由这里统一。
 *
 * 为什么不做成「另一个路由页」：相册只有一个入口，真跳页要做状态恢复；
 * 用视图切换 + 返回，列表滚动位置由宿主保存/还原。
 */
const props = defineProps({
  /** { key, label, subtitle, address, count, coverPath } —— 与相册卡字段同构，三类相册通用 */
  album: { type: Object, required: true },
  /** 成员照片绝对路径（顺序无所谓，组件按文件名倒序排） */
  photos: { type: Array, default: () => [] },
  /** 返回按钮文字：地点 → 「地点相册」，人物 → 「人物相册」… */
  backLabel: { type: String, default: "相册列表" },
  /** 相册类型名，用于标题前的弱化标签，可空 */
  kindLabel: { type: String, default: "" },
  loading: { type: Boolean, default: false },
  /** 读取失败文案（空 = 正常）。失败不能伪装成「这个相册没有照片」 */
  loadError: { type: String, default: "" },
  emptyText: { type: String, default: "这个相册里还没有照片。" },
  /** 语境动作：追加到右键菜单，如人物相册的「取消关联」。不传 = 地点相册原行为 */
  contextActions: { type: Array, default: () => [] },
  /** 信息条按钮：[{ key, label, tone? }]，点击 emit action({key, path}) */
  infoActions: { type: Array, default: () => [] },
  /** 宿主动作反馈（如「已取消关联」）；空 = 不显示 */
  notice: { type: String, default: "" },
  noticeTone: { type: String, default: "ok" }, // ok | info | bad
});
const emit = defineEmits(["back", "set-cover", "rename", "action", "retry"]);

const RAW_EXT = new Set(["rw2", "cr2", "cr3", "nef", "arw", "dng", "raf", "orf", "rwl", "srw"]);
const active = ref(null);
const info = ref(null);
const infoLoading = ref(false);
const failed = ref(new Set());
const renaming = ref(false);
const renameValue = ref("");
const renameInput = ref(null);
let infoReq = 0;

/** 相机文件名递增 ≈ 拍摄顺序，零成本排序；要真实时间排序需后端补字段 */
const sorted = computed(() =>
  [...props.photos].sort((a, b) => basename(b).localeCompare(basename(a), "zh")),
);

/**
 * 封面按**完整路径**判等：同一次快门的 JPG 与 RAW 是两张独立照片，
 * 只有被设为封面的那一个文件打「封面」角标（docs §32）。路径分隔符与大小写归一化。
 */
function normPath(p) {
  return String(p ?? "").replaceAll("/", "\\").toLowerCase();
}
const coverKey = computed(() => normPath(props.album?.coverPath));
const isCover = (path) => !!coverKey.value && normPath(path) === coverKey.value;

/** RAW 角标：与 JPG 并列显示时用来区分，不参与任何去重 */
const RAW_SET = new Set(["rw2", "dng", "nef", "cr2", "cr3", "arw", "orf", "rwl", "raf", "srw"]);
const isRaw = (p) => RAW_SET.has(extOf(p));

function basename(p) {
  return String(p ?? "").split(/[\\/]/).pop() ?? "";
}
function extOf(p) {
  return (basename(p).split(".").pop() ?? "").toLowerCase();
}
function kindOf(p) {
  return RAW_EXT.has(extOf(p)) ? "raw" : "image";
}
function thumbUrl(path) {
  return `http://thumb.localhost/${encodeURIComponent(path)}?v=2`;
}

function num(v) {
  return Number(Number(v).toFixed(1));
}
function shutterText(ex) {
  if (!ex?.num || !ex?.den) return null;
  const secs = ex.num / ex.den;
  if (secs >= 1) return `${Number(secs.toFixed(1))}s`;
  const inv = ex.den / ex.num;
  return Math.abs(inv - Math.round(inv)) < 0.02 ? `1/${Math.round(inv)}` : `${secs.toFixed(1)}s`;
}

/** 单行信息：时间 · 尺寸 · 光圈 · 快门 · ISO · 机身/镜头 */
const metaText = computed(() => {
  const d = info.value;
  if (!d) return "";
  const out = [];
  if (d.shootTime) out.push(String(d.shootTime).replace(" ", "　"));
  if (d.width && d.height) out.push(`${d.width}×${d.height}`);
  if (d.fNumber) out.push(`f/${num(d.fNumber)}`);
  const s = shutterText(d.exposure);
  if (s) out.push(s);
  if (d.iso) out.push(`ISO ${d.iso}`);
  if (d.focalLength) out.push(`${num(d.focalLength)}mm`);
  const gear = [d.model || d.make, d.lens].filter(Boolean).join(" + ");
  if (gear) out.push(gear);
  return out.join(" · ");
});

async function loadInfo(path) {
  const req = ++infoReq;
  infoLoading.value = true;
  try {
    const d = await invoke("gallery_info", { path, kind: kindOf(path) });
    if (req === infoReq) info.value = d;
  } catch {
    if (req === infoReq) info.value = null;
  } finally {
    if (req === infoReq) infoLoading.value = false;
  }
}

function pick(path) {
  active.value = path;
  info.value = null;
  if (path) loadInfo(path);
}

async function openFile(path) {
  if (!path) return;
  try {
    await invoke("gallery_open", { path });
  } catch {
    /* 系统没有默认关联程序时静默失败，不打断浏览 */
  }
}

function markFailed(path) {
  const next = new Set(failed.value);
  next.add(path);
  failed.value = next;
}

// ── 右键菜单（与图库同一组件 PhotoMenu，docs §33）──────────────────────────
// 图库有「标记废片」，相册这边换成相册语境才有的「设为封面」。
const CTX_W = 200;
const ctxOpen = ref(false);
const ctxPos = ref({ top: 0, left: 0 });
const ctxPath = ref(null);
const actionError = ref("");
let errTimer = 0;

function flashError(e) {
  actionError.value = String(e ?? "操作失败").replace(/^Error:\s*/, "");
  clearTimeout(errTimer);
  errTimer = setTimeout(() => (actionError.value = ""), 5000);
}

function onTileContextMenu(path, e) {
  e.preventDefault(); // 屏蔽 WebView2 原生菜单
  pick(path); // 右键同时选中，信息条跟着切到这张
  ctxPath.value = path;
  // 与图库同一套夹取：4 项菜单实测 174px，留 176 的余量；
  // 语境动作每项约 30px，随 contextActions 数量一起抬高，避免菜单底部被裁。
  const h = 176 + props.contextActions.length * 30;
  ctxPos.value = {
    left: Math.max(8, Math.min(e.clientX, window.innerWidth - CTX_W - 8)),
    top: Math.max(8, Math.min(e.clientY, window.innerHeight - h - 8)),
  };
  ctxOpen.value = true;
}

const ctxMenuItems = computed(() => {
  const p = ctxPath.value;
  const covered = p ? isCover(p) : false;
  return [
    { key: "open", label: "打开", icon: ExternalLink },
    { key: "photoshop", label: "使用 Photoshop 打开", icon: WandSparkles },
    {
      key: "cover",
      label: covered ? "已是当前封面" : "设为封面",
      icon: Star,
      disabled: covered,
      separatorBefore: true,
    },
    { key: "folder", label: "打开所在文件夹", icon: FolderOpen, separatorBefore: true },
    ...props.contextActions,
  ];
});

async function onCtxPick(key) {
  const p = ctxPath.value;
  ctxOpen.value = false;
  if (!p) return;
  // 语境动作交给宿主处理（人物相册的取消关联等），组件本身不碰业务数据
  if (props.contextActions.some((a) => a.key === key)) {
    emit("action", { key, path: p });
    return;
  }
  try {
    if (key === "open") await invoke("gallery_open", { path: p });
    else if (key === "photoshop") await invoke("open_with_photoshop", { paths: [p] });
    else if (key === "cover") emit("set-cover", p);
    else if (key === "folder") await invoke("open_in_explorer", { path: p });
  } catch (e) {
    flashError(e);
  }
}

/** 与图库一致用 pointerdown：比 click 更早收起，且不会和格子的选中点击抢事件 */
function onGlobalPointerDown(e) {
  if (ctxOpen.value && !e.target.closest?.("[data-ctx-menu]")) ctxOpen.value = false;
}
function onWindowScroll() {
  if (ctxOpen.value) ctxOpen.value = false;
}

function startRename() {
  renameValue.value = props.album?.label || "";
  renaming.value = true;
  nextTick(() => {
    renameInput.value?.focus();
    renameInput.value?.select?.();
  });
}
function commitRename() {
  const v = renameValue.value.trim();
  renaming.value = false;
  if (v && v !== props.album?.label) emit("rename", v);
}

function onKey(e) {
  const tag = e.target?.tagName;
  if (tag === "INPUT" || tag === "TEXTAREA") return;
  if (e.key === "Escape" || e.key === "Backspace") {
    e.preventDefault();
    // 菜单优先：Esc 先收菜单，再按一次才返回列表
    if (ctxOpen.value) {
      ctxOpen.value = false;
      return;
    }
    emit("back");
    return;
  }
  if (!sorted.value.length) return;
  const i = active.value ? sorted.value.indexOf(active.value) : -1;
  if (e.key === "ArrowRight" || e.key === "ArrowDown") {
    e.preventDefault();
    pick(sorted.value[Math.min(sorted.value.length - 1, i + 1)]);
  } else if (e.key === "ArrowLeft" || e.key === "ArrowUp") {
    e.preventDefault();
    pick(sorted.value[Math.max(0, i - 1)]);
  } else if (e.key === "Enter" && active.value) {
    e.preventDefault();
    openFile(active.value);
  }
}

watch(
  () => props.album?.key,
  () => {
    active.value = null;
    info.value = null;
    failed.value = new Set();
    renaming.value = false;
  },
);

// 成员变化（如取消关联）后，选中项可能已不在列表里：清掉，别让信息栏
// 继续显示一张已被移出的照片。
watch(
  () => props.photos,
  (list) => {
    if (active.value && !list.includes(active.value)) {
      active.value = null;
      info.value = null;
    }
  },
);

onMounted(() => {
  window.addEventListener("keydown", onKey);
  window.addEventListener("pointerdown", onGlobalPointerDown);
  window.addEventListener("scroll", onWindowScroll, true); // 捕获阶段：任何祖先滚动都收起菜单
});
onBeforeUnmount(() => {
  window.removeEventListener("keydown", onKey);
  window.removeEventListener("pointerdown", onGlobalPointerDown);
  window.removeEventListener("scroll", onWindowScroll, true);
  clearTimeout(errTimer);
});
</script>

<template>
  <div class="mx-auto w-full max-w-[1500px] px-6 pb-6">
    <!-- 头部 + 信息条一起吸顶：滚到相册深处时返回与「设为封面」仍在手边 -->
    <div class="sticky top-0 z-10 -mx-6 border-b border-line bg-app px-6 pb-2 pt-4">
      <div class="flex flex-wrap items-center gap-x-2 gap-y-2">
        <button
          type="button"
          class="flex h-7 shrink-0 items-center gap-1 rounded-lg border border-line bg-surface pl-1.5 pr-2.5 text-xs text-ink-2 transition-colors hover:bg-hover hover:text-ink"
          :title="`返回${backLabel}（Esc）`"
          @click="emit('back')"
        >
          <ArrowLeft class="size-3.5" />
          {{ backLabel }}
        </button>

        <template v-if="renaming">
          <input
            ref="renameInput"
            v-model="renameValue"
            class="h-7 min-w-[12rem] flex-1 rounded-lg border border-line bg-surface px-2 text-sm"
            maxlength="40"
            @keyup.enter="commitRename"
            @keyup.esc="renaming = false"
          />
          <button class="grid size-7 place-items-center rounded-lg border border-line bg-surface hover:bg-hover" title="保存" @click="commitRename">
            <Check class="size-4 text-ok" />
          </button>
          <button class="grid size-7 place-items-center rounded-lg border border-line bg-surface hover:bg-hover" title="取消" @click="renaming = false">
            <X class="size-4" />
          </button>
        </template>

        <template v-else>
          <span v-if="kindLabel" class="shrink-0 rounded-md bg-amber-soft px-1.5 py-0.5 text-[11px] text-amber-text">{{ kindLabel }}</span>
          <h2 class="min-w-0 truncate text-sm font-medium text-ink">{{ album.label }}</h2>
          <span class="shrink-0 text-xs tabular-nums text-ink-2">{{ album.count }} 张</span>
          <span class="min-w-0 truncate text-xs text-ink-2" :title="album.address || album.subtitle">
            {{ album.subtitle }}
          </span>
          <span class="ml-auto flex shrink-0 items-center gap-1.5">
            <slot name="actions" />
            <button
              class="flex h-7 items-center gap-1 rounded-lg border border-line bg-surface px-2 text-xs text-ink-2 transition-colors hover:bg-hover hover:text-ink"
              title="重命名相册（仅改显示名，不改文件）"
              @click="startRename"
            >
              <Pencil class="size-3.5" /> 重命名
            </button>
          </span>
        </template>
      </div>

      <!-- 信息条：固定高度，未选中时给操作提示，避免内容跳动 -->
      <div class="mt-2 h-8">
        <div
          v-if="active"
          class="flex h-8 items-center gap-2 rounded-lg border border-line bg-surface px-2.5 text-[11px]"
        >
          <FileImage class="size-3.5 shrink-0 text-ink-3" />
          <span class="max-w-[24rem] shrink-0 truncate font-medium text-ink" :title="active">{{ basename(active) }}</span>
          <span class="min-w-0 flex-1 truncate text-ink-2" :title="metaText">
            {{ infoLoading ? "读取信息…" : metaText || "—" }}
          </span>
          <span v-if="isCover(active)" class="flex shrink-0 items-center gap-1 rounded-md bg-amber-soft px-1.5 py-0.5 text-amber-text">
            <Star class="size-3 fill-current" /> 当前封面
          </span>
          <button
            v-else
            type="button"
            class="h-6 shrink-0 rounded-md bg-amber-soft px-2 text-[11px] text-amber-text transition-colors hover:bg-amber-deep/20"
            @click="emit('set-cover', active)"
          >设为封面</button>
          <button
            type="button"
            class="flex h-6 shrink-0 items-center gap-1 rounded-md px-1.5 text-[11px] text-ink-2 hover:bg-hover"
            title="用系统看图打开（双击缩略图同效）"
            @click="openFile(active)"
          >打开</button>
          <button
            v-for="a in infoActions"
            :key="a.key"
            type="button"
            class="flex h-6 shrink-0 items-center gap-1 rounded-md px-1.5 text-[11px] transition-colors"
            :class="a.tone === 'bad' ? 'text-bad hover:bg-bad-soft' : 'text-ink-2 hover:bg-hover'"
            :title="a.title || a.label"
            @click="emit('action', { key: a.key, path: active })"
          >{{ a.label }}</button>
        </div>
        <div v-else class="flex h-8 items-center gap-2 text-[11px] text-ink-2">
          <span>单击选中查看信息 · 双击用系统看图打开 · 右键更多操作 · Esc 返回</span>
          <span class="ml-auto shrink-0 text-ink-3">按文件名倒序</span>
        </div>
      </div>

      <!-- 右键菜单动作失败（如未配置 Photoshop）时的就地提示，5s 自动消失 -->
      <p v-if="actionError" class="mt-1 truncate rounded-md bg-bad-soft px-2 py-1 text-[11px] text-bad" :title="actionError">
        {{ actionError }}
      </p>
      <!-- 宿主动作反馈：取消关联 / 重命名的成功、重复或失败 -->
      <p
        v-if="notice"
        class="mt-1 truncate rounded-md px-2 py-1 text-[11px]"
        :class="noticeTone === 'bad' ? 'bg-bad-soft text-bad' : noticeTone === 'info' ? 'bg-info-soft text-info' : 'bg-ok-soft text-ok'"
        :title="notice"
      >{{ notice }}</p>
    </div>

    <!-- 读取失败：明确报错 + 重试，不用空态冒充失败（docs §35.5） -->
    <div v-if="loadError" class="py-16 text-center">
      <p class="mx-auto flex max-w-md items-start gap-1.5 rounded-lg bg-bad-soft px-3 py-2 text-left text-xs leading-5 text-bad">
        <TriangleAlert class="mt-0.5 size-3.5 shrink-0" /> 读取照片失败：{{ loadError }}
      </p>
      <button
        type="button"
        class="mx-auto mt-3 flex h-8 items-center gap-1.5 rounded-lg border border-line bg-surface px-3 text-xs text-ink-2 transition-colors hover:bg-hover"
        @click="emit('retry')"
      >
        <RefreshCw class="size-3.5" /> 重试
      </button>
    </div>
    <div v-else-if="loading" class="py-16 text-center text-xs text-ink-2">读取照片…</div>
    <div
      v-else-if="sorted.length"
      class="mt-3 grid grid-cols-3 gap-1.5 sm:grid-cols-4 md:grid-cols-5 lg:grid-cols-6 2xl:grid-cols-8"
    >
      <div
        v-for="path in sorted"
        :key="path"
        role="button"
        tabindex="0"
        class="group relative aspect-square cursor-default overflow-hidden rounded-lg border bg-subtle transition-colors"
        :class="active === path
          ? 'border-amber ring-2 ring-amber/35'
          : isCover(path)
            ? 'border-amber/70'
            : 'border-line hover:border-line-strong'"
        :aria-label="basename(path)"
        :title="basename(path)"
        @click="pick(path)"
        @dblclick="openFile(path)"
        @contextmenu="onTileContextMenu(path, $event)"
        @keydown.enter.prevent="openFile(path)"
      >
        <img
          v-if="!failed.has(path)"
          :src="thumbUrl(path)"
          class="size-full object-cover"
          loading="lazy"
          decoding="async"
          draggable="false"
          alt=""
          @error="markFailed(path)"
        />
        <div v-else class="grid size-full place-items-center text-ink-3">
          <ImageOff class="size-6" />
        </div>

        <!-- 左下角标组：格式在前、状态在后，与图库的 MP4/RAW 角标同一位置（docs §32.2） -->
        <div class="pointer-events-none absolute bottom-1 left-1 flex items-center gap-1">
          <span
            v-if="isRaw(path)"
            class="flex items-center gap-1 rounded bg-[rgba(30,26,22,.92)] px-1.5 py-0.5 text-[10px] font-medium tracking-wide text-white"
          >
            <Aperture class="size-3" /> RAW
          </span>
          <span
            v-if="isCover(path)"
            class="rounded bg-[rgba(30,26,22,.92)] px-1.5 py-0.5 text-[10px] text-white"
          >封面</span>
        </div>

        <button
          v-if="!isCover(path)"
          type="button"
          class="absolute right-1 top-1 hidden h-6 items-center rounded-md bg-[rgba(30,26,22,.75)] px-1.5 text-[10px] text-white group-hover:flex hover:bg-[rgba(30,26,22,.92)]"
          title="把这张设为相册封面"
          @click.stop="emit('set-cover', path)"
          @dblclick.stop
        >设为封面</button>
      </div>
    </div>
    <p v-else class="py-16 text-center text-xs leading-5 text-ink-2">
      {{ emptyText }}
    </p>

    <PhotoMenu
      :open="ctxOpen"
      :pos="ctxPos"
      :title="ctxPath ? basename(ctxPath) : ''"
      :items="ctxMenuItems"
      @pick="onCtxPick"
    />
  </div>
</template>
