<script setup>
import { computed, nextTick, onMounted, ref } from "vue";
import { invoke, Channel } from "@tauri-apps/api/core";
import {
  Search,
  Image as ImageIcon,
  Map as MapIcon,
  Users,
  PawPrint,
  MapPin,
  FolderOpen,
  RefreshCw,
  TriangleAlert,
  ScanSearch,
  Languages,
  X,
} from "lucide-vue-next";
import AlbumCard from "../components/AlbumCard.vue";
import AlbumDetail from "../components/AlbumDetail.vue";
import MapView from "../components/MapView.vue";

/** 智能相册（docs §19 / §21 / §22 / §24 / §30）。地点相册与地图全部使用真实 GPS 数据。 */
const view = ref("albums"); // albums | map | detail（detail 由 AlbumDetail 统一承载，人物/宠物同构）
const rootEl = ref(null);
let listScroll = 0;

/**
 * 三类相册共用同一套详情行为（docs §30），这里只区分文案。
 * 以后接人物 / 宠物：把 albumKind 设为 person / pet、复用 openAlbum 即可，
 * **不要在列表页里另写内联铺图**。
 */
const ALBUM_KINDS = {
  place: { backLabel: "地点相册", tag: "地点" },
  person: { backLabel: "人物相册", tag: "人物" },
  pet: { backLabel: "宠物相册", tag: "宠物" },
};
const albumKind = ref("place");
const kindMeta = computed(() => ALBUM_KINDS[albumKind.value] ?? ALBUM_KINDS.place);
const models = ref([]);
const places = ref([]); // 65 个 GPS 簇（原始点位，不因显示级别改变）
const overrides = ref({});
const dbPath = ref("");
const loading = ref(true);
const error = ref("");
const scanning = ref(false);
const summary = ref(null);
const geocoding = ref(false);
const geoSummary = ref(null);
const geoProgress = ref(null);
const settings = ref({ keySet: false, level: "city" });
const selectedKey = ref(null);
const photos = ref([]);
const photosLoading = ref(false);

const cnclipReady = computed(
  () => models.value.find((m) => m.key === "cnclip")?.ready === true,
);
const facesReady = computed(
  () => models.value.find((m) => m.key === "faces")?.ready === true,
);

/**
 * 天地图对直辖市（北京 / 上海 / 天津 / 重庆）常返回 province=北京市、city 为空。
 * 显示层按「省 → 市 → 县」互相兜底归一化，已入库的旧数据无需重新解析、
 * 不消耗任何配额；同时同级同名会正确合并成一张相册卡。
 */
function normAdmin(o) {
  const p = (o.province || "").trim();
  const c = (o.city || "").trim();
  const d = (o.county || "").trim();
  return {
    province: p || c || d,
    city: c || p || d,
    county: d || c || p,
  };
}
/** 单个 GPS 簇在当前显示级别下的名字；完全未解析时回退坐标名。 */
function levelLabel(p) {
  const a = normAdmin(p);
  const lv = settings.value.level;
  const v = lv === "province" ? a.province : lv === "city" ? a.city : a.county;
  return v || p.name;
}
function adminText(o) {
  const a = normAdmin(o);
  const parts = [...new Set([a.province, a.city, a.county].filter(Boolean))];
  return parts.length
    ? parts.join(" · ")
    : `${(o.lat ?? 0).toFixed(3)}°, ${(o.lon ?? 0).toFixed(3)}°`;
}

/**
 * 按当前显示级别把 GPS 簇合并成相册：同级同名归到一张卡（§24）。
 * 存储层仍是 65 个簇（各自保留坐标与详细地址），这里只做展示聚合，
 * 切换省 / 市 / 县不用重新扫描或重新解析。
 */
const groups = computed(() => {
  const map = new Map();
  for (const p of places.value) {
    const base = levelLabel(p);
    const key = `${settings.value.level}:${base}`;
    if (!map.has(key)) {
      map.set(key, {
        key,
        label: base,
        ids: [],
        clusters: [],
        count: 0,
        coverPath: "",
        coverCount: -1,
        province: "",
        city: "",
        county: "",
        address: "",
        lat: p.lat,
        lon: p.lon,
      });
    }
    const g = map.get(key);
    g.ids.push(p.id);
    g.clusters.push({ id: p.id, lat: p.lat, lon: p.lon, count: p.count, address: p.address });
    g.count += p.count;
    // 封面优先取成员最多的簇，保证大相册的门面更有代表性
    if (p.coverPath && p.count > g.coverCount) {
      g.coverPath = p.coverPath;
      g.coverCount = p.count;
    }
    if (!g.province && p.province) g.province = p.province;
    if (!g.city && p.city) g.city = p.city;
    if (!g.county && p.county) g.county = p.county;
    if (!g.address && p.address) g.address = p.address;
  }
  const out = [];
  for (const g of map.values()) {
    g.label = overrides.value[`group:alias:${g.key}`] || g.label;
    const cover = overrides.value[`group:cover:${g.key}`];
    if (cover) g.coverPath = cover;
    g.subtitle = adminText(g);
    out.push(g);
  }
  return out.sort((a, b) => b.count - a.count);
});

const selectedGroup = computed(
  () => groups.value.find((g) => g.key === selectedKey.value) || null,
);
const pendingGeocode = computed(
  () => places.value.filter((p) => !p.geocoded).length,
);
const addressCount = computed(
  () => places.value.filter((p) => p.address).length,
);
const geocodeBtnText = computed(() => {
  if (geocoding.value) return "解析中…";
  if (pendingGeocode.value === 0) return "已全部解析";
  return pendingGeocode.value < places.value.length
    ? `解析地名 · 补 ${pendingGeocode.value}`
    : "解析地名";
});
const geocodePct = computed(() => {
  const p = geoProgress.value;
  return p && p.total ? Math.round((p.done / p.total) * 100) : 0;
});

// ── 提示条：只承载「本次动作的结果」（扫描 / 解析），不再常驻状态提示。
//    用户反馈：每次打开都提示「还有 N 个点位未缓存地址」是噪音（那 1 个点位是天地图
//    本身返回空区划，重试也不会成功）；解析状态改由按钮文案与统计行表达。────────────
const showTipGroup = computed(
  () => !loading.value && (summary.value !== null || geoSummary.value !== null),
);
function clearTips() {
  summary.value = null;
  geoSummary.value = null;
}

async function loadPhotos(g) {
  if (!g) return;
  try {
    photos.value = await invoke("smart_group_photos", { ids: g.ids });
  } catch (e) {
    error.value = String(e);
  }
}

async function refresh() {
  loading.value = true;
  error.value = "";
  try {
    const [m, p, d, s, o] = await Promise.all([
      invoke("smart_model_status"),
      invoke("smart_places"),
      invoke("smart_db_path"),
      invoke("smart_get_settings"),
      invoke("smart_group_overrides"),
    ]);
    models.value = m || [];
    places.value = p || [];
    dbPath.value = d || "";
    settings.value = s || { keySet: false, level: "city" };
    overrides.value = o || {};
    if (selectedKey.value) {
      const g = groups.value.find((x) => x.key === selectedKey.value);
      if (g) await loadPhotos(g);
      else {
        selectedKey.value = null;
        photos.value = [];
        if (view.value === "detail") view.value = "albums";
      }
    }
  } catch (e) {
    error.value = String(e);
  } finally {
    loading.value = false;
  }
}

async function scan() {
  scanning.value = true;
  error.value = "";
  try {
    summary.value = await invoke("smart_scan_gps");
    await refresh();
  } catch (e) {
    error.value = String(e);
  } finally {
    scanning.value = false;
  }
}

/** 仅用户主动点击 + 已填 key 时联网；进度经 Channel 推送，结果仍由 invoke 返回。 */
async function geocode() {
  geocoding.value = true;
  error.value = "";
  geoSummary.value = null;
  geoProgress.value = { done: 0, total: 0, ok: 0, failed: 0, current: "准备中" };
  const onProgress = new Channel();
  onProgress.onmessage = (p) => {
    geoProgress.value = p;
  };
  try {
    geoSummary.value = await invoke("smart_geocode_places", { onProgress });
    await refresh();
  } catch (e) {
    error.value = String(e);
  } finally {
    geocoding.value = false;
    geoProgress.value = null;
  }
}

async function selectGroup(key) {
  selectedKey.value = key;
  await loadPhotos(selectedGroup.value);
}

/**
 * 进入相册详情（docs §30）：所有相册类型共用一套行为 —— 点卡片进详情、左上角返回。
 * 记住列表滚动位置，返回时还原，避免从 60 多个相册里回来又从头找。
 */
async function openAlbum(g, kind = "place") {
  listScroll = scroller()?.scrollTop ?? 0;
  albumKind.value = kind;
  selectedKey.value = g.key;
  photos.value = [];
  photosLoading.value = true;
  view.value = "detail";
  await loadPhotos(g);
  photosLoading.value = false;
}

function backToAlbums() {
  view.value = "albums";
  nextTick(() => {
    const s = scroller();
    if (s) s.scrollTop = listScroll;
  });
}

/** 相册列表的滚动容器在 App.vue（主区 overflow-y-auto），用标记属性拿到它 */
function scroller() {
  return rootEl.value?.closest("[data-scroll-root]") ?? null;
}

async function setCover(path) {
  if (!selectedKey.value) return;
  try {
    await invoke("smart_set_group_override", {
      key: `group:cover:${selectedKey.value}`,
      value: path,
    });
    await refresh();
  } catch (e) {
    error.value = String(e);
  }
}

/** 重命名只改显示名（override），不动文件、不动索引 */
async function renameAlbum(name) {
  if (!selectedKey.value || !name) return;
  try {
    await invoke("smart_set_group_override", {
      key: `group:alias:${selectedKey.value}`,
      value: name,
    });
    await refresh();
  } catch (e) {
    error.value = String(e);
  }
}

async function openModels() {
  try {
    await invoke("smart_open_models_dir");
  } catch (e) {
    error.value = String(e);
  }
}

onMounted(refresh);
</script>

<template>
  <div ref="rootEl">
    <!-- 相册详情：三类相册统一走这一个组件（docs §30），列表视图整体让位 -->
    <AlbumDetail
      v-if="view === 'detail' && selectedGroup"
      :album="selectedGroup"
      :photos="photos"
      :loading="photosLoading"
      :back-label="kindMeta.backLabel"
      :kind-label="kindMeta.tag"
      @back="backToAlbums"
      @set-cover="setCover"
      @rename="renameAlbum"
    >
      <template #actions>
        <button
          type="button"
          class="flex h-7 items-center gap-1 rounded-lg border border-line bg-surface px-2 text-xs text-ink-2 transition-colors hover:bg-hover hover:text-ink"
          title="切换地图视图并定位这个相册"
          @click="view = 'map'"
        >
          <MapIcon class="size-3.5" /> 在地图中查看
        </button>
      </template>
    </AlbumDetail>

    <div v-else class="mx-auto max-w-6xl space-y-4 p-6">
    <!-- 视图切换（§19.5 方案 B） -->
    <div class="flex flex-wrap items-center gap-2">
      <div class="inline-flex rounded-lg border border-line bg-surface p-0.5 text-sm">
        <button
          class="flex h-8 items-center gap-1.5 rounded-md px-3 transition-colors"
          :class="view === 'albums' ? 'bg-amber-soft font-medium text-ink' : 'text-ink-2 hover:bg-hover'"
          @click="view = 'albums'"
        >
          <ImageIcon class="size-4" /> 相册
        </button>
        <button
          class="flex h-8 items-center gap-1.5 rounded-md px-3 transition-colors"
          :class="view === 'map' ? 'bg-amber-soft font-medium text-ink' : 'text-ink-2 hover:bg-hover'"
          @click="view = 'map'"
        >
          <MapIcon class="size-4" /> 地图
        </button>
      </div>
      <span class="text-xs text-ink-2">按人 / 宠物 / 地点组织，支持中文语义搜图</span>
      <div class="ml-auto flex flex-wrap gap-2">
        <button
          class="flex h-8 items-center gap-1.5 rounded-lg border border-line px-3 text-xs text-ink-2 transition-colors hover:bg-hover disabled:opacity-45"
          :disabled="!settings.keySet || geocoding || pendingGeocode === 0"
          :title="!settings.keySet
            ? '请先到设置 → 在线地点服务填写天地图 key'
            : pendingGeocode === 0
              ? '全部点位已解析并缓存，再点击不会发出任何请求、不消耗配额'
              : `还有 ${pendingGeocode} 个点位待解析；已解析的会跳过，不重复消耗配额`"
          @click="geocode"
        >
          <Languages class="size-3.5" /> {{ geocodeBtnText }}
        </button>
        <button
          class="flex h-8 items-center gap-1.5 rounded-lg bg-amber px-3 text-xs font-medium text-ink ring-1 ring-inset ring-amber-deep/45 transition-colors hover:bg-amber-deep disabled:opacity-55"
          :disabled="scanning"
          @click="scan"
        >
          <ScanSearch class="size-3.5" /> {{ scanning ? "扫描中…" : "扫描 GPS" }}
        </button>
        <button class="flex h-8 items-center gap-1.5 rounded-lg border border-line px-2.5 text-xs text-ink-2 hover:bg-hover" @click="refresh">
          <RefreshCw class="size-3.5" /> 刷新
        </button>
      </div>
    </div>

    <p v-if="error" class="flex items-center gap-1.5 rounded-card border border-line bg-bad-soft px-3 py-2 text-xs text-bad">
      <TriangleAlert class="size-3.5" /> {{ error }}
    </p>

    <!-- 结果提示组：只显示这一次「扫描 / 解析地名」的结果，打开页面不会常驻任何提示 -->
    <div v-if="showTipGroup" class="flex items-start gap-2">
      <div class="min-w-0 flex-1 space-y-2">
        <p v-if="summary" class="rounded-lg bg-ok-soft px-3 py-2 text-xs text-ok">
          扫描 {{ summary.scanned }} 个文件 · {{ summary.withGps }} 张带 GPS 坐标 · 生成 {{ summary.places }} 个 GPS 点位
        </p>
        <p v-if="geoSummary" class="rounded-lg bg-info-soft px-3 py-2 text-xs text-info">
          解析 {{ geoSummary.total }} 个地点 · 成功 {{ geoSummary.ok }} · 失败 {{ geoSummary.failed }}
          <span v-if="geoSummary.message"> · {{ geoSummary.message }}</span>
        </p>
      </div>
      <button
        type="button"
        class="flex h-7 shrink-0 items-center gap-1 rounded-lg border border-line bg-surface px-2.5 text-[11px] text-ink-2 transition-colors hover:bg-hover"
        title="收起这次扫描 / 解析的结果提示"
        @click="clearTips"
      >
        <X class="size-3" /> 清除
      </button>
    </div>

    <!-- 地名解析进度：后端 Channel 推送，显示已处理数 / 成功 / 失败 / 当前点位 -->
    <section
      v-if="geocoding && geoProgress"
      class="rounded-card border border-line bg-surface p-3.5 shadow-card"
      aria-live="polite"
    >
      <div class="mb-2 flex flex-wrap items-center justify-between gap-2 text-xs">
        <span class="font-medium text-ink">
          正在解析地名 · {{ geoProgress.done }} / {{ geoProgress.total || "…" }}
        </span>
        <span class="text-ink-2">成功 {{ geoProgress.ok }} · 失败 {{ geoProgress.failed }}</span>
      </div>
      <div class="h-1.5 overflow-hidden rounded-full bg-subtle">
        <div
          class="h-full rounded-full bg-amber transition-[width] duration-150 ease-out"
          :style="{ width: geocodePct + '%' }"
        ></div>
      </div>
      <p class="mt-2 text-[11px] leading-4 text-ink-3">
        当前点位 {{ geoProgress.current }} · 每个点位间隔 350ms 以避免触发天地图限流
      </p>
    </section>

    <!-- 语义搜索门禁：推理未接入前如实标注，不做假输入框 -->
    <section class="rounded-card border border-line bg-surface p-4 shadow-card">
      <div class="flex h-10 items-center gap-2 rounded-lg border border-line bg-subtle px-3 opacity-70">
        <Search class="size-4 shrink-0 text-ink-3" />
        <span class="truncate text-sm text-ink-2">
          {{ cnclipReady ? "模型已就绪，中文语义推理将在第四批接入" : "下载模型后可用语义搜图" }}
        </span>
      </div>
      <div v-if="!cnclipReady" class="mt-2 flex flex-wrap items-center gap-2 text-xs text-ink-2">
        <span>语义搜图模型未就绪</span>
        <button class="flex h-7 items-center gap-1.5 rounded-lg border border-line px-2.5 hover:bg-hover" @click="openModels">
          <FolderOpen class="size-3.5" /> 打开模型目录
        </button>
      </div>
    </section>

    <!-- 相册视图 -->
    <template v-if="view === 'albums'">
      <div class="grid gap-3 sm:grid-cols-3">
        <section class="rounded-card border border-line bg-surface p-3.5 shadow-card">
          <div class="mb-1 flex items-center gap-2 text-xs font-medium"><Users class="size-3.5 text-ink-3" /> 人物</div>
          <p class="text-[11px] leading-5 text-ink-2">{{ facesReady ? "模型已就绪，识别能力第五批接入" : "模型未下载，下载后开放" }}</p>
        </section>
        <section class="rounded-card border border-line bg-surface p-3.5 shadow-card">
          <div class="mb-1 flex items-center gap-2 text-xs font-medium"><PawPrint class="size-3.5 text-ink-3" /> 宠物</div>
          <p class="text-[11px] leading-5 text-ink-2">手动建册，不自动识别；第五批接入</p>
        </section>
        <section class="rounded-card border border-line bg-surface p-3.5 shadow-card">
          <div class="mb-1 flex items-center gap-2 text-xs font-medium"><MapPin class="size-3.5 text-ink-3" /> 地点</div>
          <p class="text-[11px] leading-5 text-ink-2">
            {{ places.length }} 个 GPS 点位 · 归为 {{ groups.length }} 个{{ settings.level === "province" ? "省" : settings.level === "city" ? "市" : "县" }}级相册 · 详细地址 {{ addressCount }} 条
          </p>
        </section>
      </div>

      <section class="rounded-card border border-line bg-surface p-4 shadow-card">
        <h2 class="mb-1 text-sm font-medium">地点相册</h2>
        <p v-if="!settings.keySet" class="mb-3 text-[11px] text-ink-2">
          当前显示 GPS 坐标；到「设置 → 在线地点服务」填写天地图 key 后点「解析地名」，即可显示省 / 市 / 县。不填也能手动重命名。
        </p>
        <div v-if="loading" class="text-xs text-ink-2">读取中…</div>
        <div v-else-if="groups.length" class="grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-4">
          <AlbumCard
            v-for="g in groups"
            :key="g.key"
            :title="g.label"
            :subtitle="g.subtitle"
            :count="g.count"
            :cover-path="g.coverPath || ''"
            :selected="g.key === selectedKey"
            @select="openAlbum(g)"
          />
        </div>
        <p v-else class="text-xs leading-5 text-ink-2">
          还没有已入库的 GPS 点位。点「扫描 GPS」读取媒体库照片的 EXIF 坐标；
          没有 GPS 的照片不会进入地点相册，也不会猜测位置。
        </p>
      </section>

      <!-- 相册详情已抽成通用视图（docs §30）：人物 / 宠物 / 地点共用，列表页不再内联铺图 -->
    </template>

    <!-- 地图视图 -->
    <section v-else class="rounded-card border border-line bg-surface p-4 shadow-card">
      <MapView :groups="groups" :selected-key="selectedKey" @select="selectGroup" />
    </section>

    <p class="text-xs text-ink-3">
      智能相册索引：<span class="break-all">{{ dbPath || "—" }}</span>（独立于评分数据库，删除即回到无 AI 状态）
    </p>
    </div>
  </div>
</template>
