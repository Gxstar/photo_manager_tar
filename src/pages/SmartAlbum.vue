<script setup>
import { computed, nextTick, onBeforeUnmount, onMounted, ref } from "vue";
import { invoke, Channel } from "@tauri-apps/api/core";
import { ask } from "@tauri-apps/plugin-dialog";
import {
  Image as ImageIcon,
  Map as MapIcon,
  Users,
  UserMinus,
  MapPin,
  Plus,
  Trash2,
  RefreshCw,
  TriangleAlert,
  ScanSearch,
  Languages,
  X,
} from "lucide-vue-next";
import AlbumCard from "../components/AlbumCard.vue";
import AlbumDetail from "../components/AlbumDetail.vue";
import MapView from "../components/MapView.vue";
import { errMsg } from "../store.js";

/** 智能相册（docs §19 / §21 / §22 / §24 / §30 / §36）。地点相册与地图全部使用真实 GPS 数据。 */
// 页内分段（§36）：一次只渲染一段，人物 / 地点 / 地图互不挤占；
// detail 时列表区整体让位给 AlbumDetail（沿用 §30 的 v-if / v-else）。
// 默认落 place：真实库已有 65 个 GPS 簇、0 个人物，先给有数据的一段。
const view = ref("place"); // person | place | map | detail
const lastListView = ref("place"); // 进入详情前的分段，返回时还原
const rootEl = ref(null);
let listScroll = 0;

/**
 * 三类相册共用同一套详情行为（docs §30），这里只区分文案。
 * 以后接人物 / 宠物：把 albumKind 设为 person / pet、复用 openAlbum 即可，
 * **不要在列表页里另写内联铺图**。
 */
const ALBUM_KINDS = {
  place: {
    backLabel: "地点相册",
    tag: "地点",
    emptyText: "这个相册里还没有照片。",
    contextActions: [],
    infoActions: [],
  },
  person: {
    backLabel: "人物相册",
    tag: "人物",
    emptyText: "这个人物下还没有照片。到「图库」选中照片，右键「关联到人物」即可加入；照片文件不会移动或改名。",
    contextActions: [{ key: "unlink", label: "取消关联", icon: UserMinus, separatorBefore: true }],
    infoActions: [{ key: "unlink", label: "取消关联" }],
  },
  pet: {
    backLabel: "宠物相册",
    tag: "宠物",
    emptyText: "这个相册里还没有照片。",
    contextActions: [],
    infoActions: [],
  },
};
const albumKind = ref("place");
const kindMeta = computed(() => ALBUM_KINDS[albumKind.value] ?? ALBUM_KINDS.place);
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

// ── 人物相册（docs §35）：手动建册，独立于 GPS 地点相册的错误通道 ──────────
const people = ref([]);
const peopleLoading = ref(false);
const peopleError = ref("");
const personNotice = ref("");
const personNoticeTone = ref("ok");
const creatingPerson = ref(false);
const newPersonName = ref("");
const personBusy = ref(false);
const personFormError = ref("");
const personNameInput = ref(null);
const photosError = ref(""); // 详情读取失败（与列表页 error 分开）
const albumNotice = ref(""); // 详情内动作反馈
const albumNoticeTone = ref("ok");
let noticeTimer = 0;
let personNoticeTimer = 0;

const personPhotoCount = computed(() =>
  people.value.reduce((s, p) => s + (p.count || 0), 0),
);
/** 人物 → 与地点相册同构的卡片对象，保证 AlbumDetail / openAlbum 完全复用 */
const personGroups = computed(() =>
  people.value.map((p) => ({
    key: `person:${p.id}`,
    id: p.id,
    kind: "person",
    label: p.name,
    subtitle: "手动关联",
    count: p.count,
    coverPath: p.coverPath || "",
  })),
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

/** 当前打开的相册：地点从 groups、人物从 personGroups，其余行为完全一致 */
const selectedGroup = computed(() => {
  if (!selectedKey.value) return null;
  const pool = albumKind.value === "person" ? personGroups.value : groups.value;
  return pool.find((g) => g.key === selectedKey.value) || null;
});

/** 分段说明只讲这一段在做什么，不复述卡片里的数字（数字在段按钮上） */
const viewHint = computed(() => {
  const hints = {
    person: "手动建册 · 到图库选中照片右键「关联到人物」",
    place: "EXIF GPS 坐标聚类 · 省 / 市 / 县分级",
    map: "WGS-84 → GCJ-02 纠偏 · 高德瓦片",
  };
  return hints[view.value] ?? "";
});

/** 地图段「在地点相册中查看」只在确实选中了地点相册时出现（人物 key 不算） */
const selectedPlace = computed(
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

/** 详情成员：地点按 GPS 簇并集，人物按 person_id 直接取；失败交给详情内错误条 */
async function loadPhotos(g) {
  if (!g) return;
  photosLoading.value = true;
  photosError.value = "";
  try {
    photos.value =
      g.kind === "person"
        ? await invoke("smart_person_photos", { id: g.id })
        : await invoke("smart_group_photos", { ids: g.ids });
  } catch (e) {
    photos.value = [];
    photosError.value = errMsg(e);
  } finally {
    photosLoading.value = false;
  }
}

/** 人物列表独立错误通道：人物读取失败不该连累地点 / 地图数据的刷新 */
async function loadPeople() {
  peopleLoading.value = true;
  peopleError.value = "";
  try {
    people.value = (await invoke("smart_people")) || [];
  } catch (e) {
    people.value = [];
    peopleError.value = errMsg(e);
  } finally {
    peopleLoading.value = false;
  }
}

async function refresh() {
  loading.value = true;
  error.value = "";
  try {
    const [p, d, s, o] = await Promise.all([
      invoke("smart_places"),
      invoke("smart_db_path"),
      invoke("smart_get_settings"),
      invoke("smart_group_overrides"),
    ]);
    places.value = p || [];
    dbPath.value = d || "";
    settings.value = s || { keySet: false, level: "city" };
    overrides.value = o || {};
  } catch (e) {
    error.value = errMsg(e);
  } finally {
    loading.value = false;
  }
  await loadPeople();
  // 详情打开时同步刷新成员（关联 / 取消关联 / 设封面 / 重命名之后）；
  // 相册已被删除（重扫 GPS 或删人物）则清掉失效选中态，不让详情挂在空数据上。
  if (view.value === "detail" && selectedGroup.value) {
    await loadPhotos(selectedGroup.value);
  } else if (view.value === "detail" && !selectedGroup.value) {
    // 详情目标已被外部删除（人物被删 / GPS 重扫重建）：回到进入前的分段，
    // 否则 view 停在 detail 却没有任何一段可渲染，整页剩下工具条和空白。
    selectedKey.value = null;
    photos.value = [];
    view.value = lastListView.value;
  } else if (selectedKey.value && !selectedGroup.value) {
    selectedKey.value = null;
    photos.value = [];
  }
}

async function scan() {
  scanning.value = true;
  error.value = "";
  try {
    summary.value = await invoke("smart_scan_gps");
    await refresh();
  } catch (e) {
    error.value = errMsg(e);
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
    error.value = errMsg(e);
  } finally {
    geocoding.value = false;
    geoProgress.value = null;
  }
}

async function selectGroup(key) {
  // 地图 / 地点列表是地点语境：从人物详情返回后 albumKind 仍是 person，
  // 不复位就会拿地点 key 去人物列表里找，选中态和成员都加载不出来。
  albumKind.value = "place";
  selectedKey.value = key;
  await loadPhotos(selectedGroup.value);
}

/**
 * 进入相册详情（docs §30）：所有相册类型共用一套行为 —— 点卡片进详情、左上角返回。
 * 记住列表滚动位置，返回时还原，避免从 60 多个相册里回来又从头找。
 */
async function openAlbum(g, kind = "place") {
  listScroll = scroller()?.scrollTop ?? 0;
  // 记住从哪一段进来：返回时回原段，而不是笼统的「相册」（§36.3 ①）
  if (view.value !== "detail") lastListView.value = view.value;
  albumKind.value = kind;
  selectedKey.value = g.key;
  photos.value = [];
  photosError.value = "";
  albumNotice.value = "";
  view.value = "detail";
  await loadPhotos(g);
}

function backToAlbums() {
  view.value = lastListView.value;
  photosError.value = "";
  albumNotice.value = "";
  nextTick(() => {
    const s = scroller();
    if (s) s.scrollTop = listScroll;
  });
}

/** 相册列表的滚动容器在 App.vue（主区 overflow-y-auto），用标记属性拿到它 */
function scroller() {
  return rootEl.value?.closest("[data-scroll-root]") ?? null;
}

function flashAlbumNotice(text, tone = "ok") {
  albumNotice.value = text;
  albumNoticeTone.value = tone;
  clearTimeout(noticeTimer);
  noticeTimer = setTimeout(() => (albumNotice.value = ""), 5000);
}

function flashPersonNotice(text, tone = "ok") {
  personNotice.value = text;
  personNoticeTone.value = tone;
  clearTimeout(personNoticeTimer);
  personNoticeTimer = setTimeout(() => (personNotice.value = ""), 6000);
}

async function setCover(path) {
  const g = selectedGroup.value;
  if (!g) return;
  try {
    if (albumKind.value === "person") {
      await invoke("smart_set_person_cover", { id: g.id, path });
    } else {
      await invoke("smart_set_group_override", {
        key: `group:cover:${g.key}`,
        value: path,
      });
    }
    await refresh();
    flashAlbumNotice("已设为封面");
  } catch (e) {
    flashAlbumNotice(errMsg(e), "bad");
  }
}

/** 地点相册只改显示名（override）；人物相册改的是 albums.name 本身 */
async function renameAlbum(name) {
  const g = selectedGroup.value;
  if (!g || !name) return;
  try {
    if (albumKind.value === "person") {
      await invoke("smart_rename_person", { id: g.id, name });
    } else {
      await invoke("smart_set_group_override", {
        key: `group:alias:${g.key}`,
        value: name,
      });
    }
    await refresh();
    flashAlbumNotice(`已重命名为「${name}」`);
  } catch (e) {
    flashAlbumNotice(errMsg(e), "bad");
  }
}

function toggleCreatePerson() {
  creatingPerson.value = !creatingPerson.value;
  newPersonName.value = "";
  personFormError.value = "";
  if (creatingPerson.value) nextTick(() => personNameInput.value?.focus());
}

async function createPerson() {
  const name = newPersonName.value.trim();
  if (!name) {
    personFormError.value = "人物名字不能为空";
    return;
  }
  personBusy.value = true;
  personFormError.value = "";
  try {
    const p = await invoke("smart_create_person", { name });
    creatingPerson.value = false;
    newPersonName.value = "";
    await loadPeople();
    flashPersonNotice(`已新建人物「${p.name}」，到图库选中照片右键「关联到人物」即可加入`);
  } catch (e) {
    personFormError.value = errMsg(e);
  } finally {
    personBusy.value = false;
  }
}

/** 删除人物 = 删索引 + 成员关系，二次确认；照片文件一律不动（docs §35.1） */
async function deletePerson() {
  const g = selectedGroup.value;
  if (!g || albumKind.value !== "person") return;
  const ok = await ask(
    `删除人物「${g.label}」？将同时清除 ${g.count} 条照片关联。\n照片文件仍原样留在媒体库，可随时重新关联；此操作不会移入回收站，也不改动任何文件。`,
    { title: "删除人物", kind: "warning", okLabel: "删除人物", cancelLabel: "取消" },
  );
  if (!ok) return;
  try {
    const n = await invoke("smart_delete_person", { id: g.id });
    view.value = "person"; // 删完回人物段，别落到已废弃的 'albums' 值上
    selectedKey.value = null;
    photos.value = [];
    await refresh();
    flashPersonNotice(`已删除「${g.label}」及 ${n} 条关联，照片文件未改动`);
  } catch (e) {
    flashPersonNotice(errMsg(e), "bad");
  }
}

/** 详情语境动作：人物相册的「取消关联」（重复操作也会如实回报） */
async function onAlbumAction({ key, path }) {
  if (key !== "unlink" || !path) return;
  const g = selectedGroup.value;
  if (!g || albumKind.value !== "person") return;
  try {
    const r = await invoke("smart_person_unlink", { id: g.id, paths: [path] });
    await refresh();
    flashAlbumNotice(
      r.removed
        ? `已从「${g.label}」移除 1 张照片（文件未改动）`
        : "这张照片本就不在该人物中",
      r.removed ? "ok" : "info",
    );
  } catch (e) {
    flashAlbumNotice(errMsg(e), "bad");
  }
}

onMounted(refresh);
onBeforeUnmount(() => {
  clearTimeout(noticeTimer);
  clearTimeout(personNoticeTimer);
});
</script>

<template>
  <div ref="rootEl">
    <!-- 相册详情：三类相册统一走这一个组件（docs §30），列表视图整体让位 -->
    <AlbumDetail
      v-if="view === 'detail' && selectedGroup"
      :album="selectedGroup"
      :photos="photos"
      :loading="photosLoading"
      :load-error="photosError"
      :empty-text="kindMeta.emptyText"
      :context-actions="kindMeta.contextActions"
      :info-actions="kindMeta.infoActions"
      :notice="albumNotice"
      :notice-tone="albumNoticeTone"
      :back-label="kindMeta.backLabel"
      :kind-label="kindMeta.tag"
      @back="backToAlbums"
      @set-cover="setCover"
      @rename="renameAlbum"
      @retry="loadPhotos(selectedGroup)"
      @action="onAlbumAction"
    >
      <template #actions>
        <button
          v-if="albumKind === 'place'"
          type="button"
          class="flex h-7 items-center gap-1 rounded-lg border border-line bg-surface px-2 text-xs text-ink-2 transition-colors hover:bg-hover hover:text-ink"
          title="切换地图视图并定位这个相册"
          @click="view = 'map'"
        >
          <MapIcon class="size-3.5" /> 在地图中查看
        </button>
        <button
          v-else-if="albumKind === 'person'"
          type="button"
          class="flex h-7 items-center gap-1 rounded-lg border border-bad/40 bg-surface px-2 text-xs text-bad transition-colors hover:bg-bad-soft"
          title="删除这个人物及其照片关联（照片文件不受影响）"
          @click="deletePerson"
        >
          <Trash2 class="size-3.5" /> 删除人物
        </button>
      </template>
    </AlbumDetail>

    <div v-else class="mx-auto max-w-6xl space-y-4 p-6">
    <!-- 页内分段（§36）：一次只渲染一段；地点专属工具只在地点段出现，人物段不再顶着 GPS 按钮 -->
    <div class="flex flex-wrap items-center gap-2">
      <div class="inline-flex rounded-lg border border-line bg-surface p-0.5 text-sm">
        <button
          type="button"
          class="flex h-8 items-center gap-1.5 rounded-md px-3 transition-colors"
          :class="view === 'person' ? 'bg-amber-soft font-medium text-ink' : 'text-ink-2 hover:bg-hover'"
          @click="view = 'person'"
        >
          <Users class="size-4" /> 人物
          <span class="text-[11px] tabular-nums" :class="view === 'person' ? 'text-amber-text' : 'text-ink-3'">{{ peopleLoading ? "…" : people.length }}</span>
        </button>
        <button
          type="button"
          class="flex h-8 items-center gap-1.5 rounded-md px-3 transition-colors"
          :class="view === 'place' ? 'bg-amber-soft font-medium text-ink' : 'text-ink-2 hover:bg-hover'"
          @click="view = 'place'"
        >
          <MapPin class="size-4" /> 地点
          <span class="text-[11px] tabular-nums" :class="view === 'place' ? 'text-amber-text' : 'text-ink-3'">{{ loading ? "…" : groups.length }}</span>
        </button>
        <button
          type="button"
          class="flex h-8 items-center gap-1.5 rounded-md px-3 transition-colors"
          :class="view === 'map' ? 'bg-amber-soft font-medium text-ink' : 'text-ink-2 hover:bg-hover'"
          @click="view = 'map'"
        >
          <MapIcon class="size-4" /> 地图
        </button>
      </div>
      <span class="text-xs text-ink-2">{{ viewHint }}</span>
      <div class="ml-auto flex flex-wrap gap-2">
        <template v-if="view === 'place'">
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
        </template>
        <button class="flex h-8 items-center gap-1.5 rounded-lg border border-line px-2.5 text-xs text-ink-2 hover:bg-hover" @click="refresh">
          <RefreshCw class="size-3.5" /> 刷新
        </button>
      </div>
    </div>

    <p v-if="error" class="flex items-center gap-1.5 rounded-card border border-line bg-bad-soft px-3 py-2 text-xs text-bad">
      <TriangleAlert class="size-3.5" /> {{ error }}
    </p>

    <!-- 以下三块只属于地点段（§36.2）：解析地名 / 扫描 GPS 只在地点段发起，进度与结果也不该出现在人物段 -->
    <!-- 结果提示组：只显示这一次「扫描 / 解析地名」的结果，打开页面不会常驻任何提示 -->
    <div v-if="view === 'place' && showTipGroup" class="flex items-start gap-2">
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
      v-if="view === 'place' && geocoding && geoProgress"
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

    <!-- 语义搜图入口已移到图库搜索浮层（docs §37）：它属于「全库找照片」，不属于地点段 -->

    <!-- 人物段（§36）：三段统计卡已删（数字在分段按钮 / 段内标题行），一次只铺一段网格 -->
    <template v-if="view === 'person'">
      <section class="rounded-card border border-line bg-surface p-4 shadow-card">
        <div class="mb-3 flex flex-wrap items-center gap-2">
          <h2 class="text-sm font-medium">人物相册</h2>
          <span class="text-[11px] text-ink-2">{{ people.length }} 个人物 · {{ personPhotoCount }} 张照片</span>
          <div class="ml-auto flex items-center gap-2">
            <button
              v-if="peopleError"
              class="flex h-7 items-center gap-1 rounded-lg border border-line px-2.5 text-xs text-ink-2 hover:bg-hover"
              @click="loadPeople"
            ><RefreshCw class="size-3.5" /> 重试</button>
            <button
              type="button"
              class="flex h-7 items-center gap-1 rounded-lg border border-line bg-surface px-2.5 text-xs text-ink-2 transition-colors hover:bg-hover hover:text-ink"
              :class="creatingPerson ? 'border-amber text-amber-text' : ''"
              @click="toggleCreatePerson"
            ><Plus class="size-3.5" /> 新建人物</button>
          </div>
        </div>

        <p v-if="personNotice" class="mb-2 rounded-lg px-3 py-2 text-xs" :class="personNoticeTone === 'bad' ? 'bg-bad-soft text-bad' : 'bg-ok-soft text-ok'">
          {{ personNotice }}
        </p>
        <p v-if="peopleError" class="mb-2 flex items-center gap-1.5 rounded-lg bg-bad-soft px-3 py-2 text-xs text-bad">
          <TriangleAlert class="size-3.5 shrink-0" /> 读取人物失败：{{ peopleError }}
        </p>

        <div v-if="creatingPerson" class="mb-3 flex items-center gap-2">
          <input
            v-model="newPersonName"
            ref="personNameInput"
            class="h-8 min-w-0 flex-1 rounded-lg border border-line bg-subtle px-2.5 text-xs outline-none focus:border-amber"
            maxlength="40"
            placeholder="人物名字，如 阿星"
            @keyup.enter="createPerson"
            @keyup.esc.stop="toggleCreatePerson"
          />
          <button
            type="button"
            class="h-8 shrink-0 rounded-lg bg-amber px-3 text-xs font-medium text-ink ring-1 ring-inset ring-amber-deep/45 transition-colors hover:bg-amber-deep disabled:opacity-50"
            :disabled="personBusy"
            @click="createPerson"
          >{{ personBusy ? "创建中…" : "创建" }}</button>
          <button type="button" class="h-8 shrink-0 rounded-lg border border-line px-2.5 text-xs text-ink-2 hover:bg-hover" @click="toggleCreatePerson">取消</button>
        </div>
        <p v-if="personFormError" class="mb-2 flex items-center gap-1.5 rounded-md bg-bad-soft px-2.5 py-1.5 text-[11px] text-bad">
          <TriangleAlert class="size-3.5 shrink-0" /> {{ personFormError }}
        </p>

        <div v-if="peopleLoading" class="text-xs text-ink-2">读取中…</div>
        <div v-else-if="personGroups.length" class="grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-4">
          <AlbumCard
            v-for="p in personGroups"
            :key="p.key"
            :title="p.label"
            :subtitle="p.subtitle"
            :count="p.count"
            :cover-path="p.coverPath"
            :selected="p.key === selectedKey"
            @select="openAlbum(p, 'person')"
          />
        </div>
        <!-- 读取失败时只显示上面的错误条 + 重试，不拿「还没有人物」冒充失败 -->
        <p v-else-if="!peopleError" class="text-xs leading-5 text-ink-2">
          还没有人物。点「新建人物」建一个，再到「图库」选中照片、右键「关联到人物」把照片加进来；
          照片文件不会被移动、改名或写回。
        </p>
      </section>
    </template>

    <!-- 地点段（§36）：统计移到段内标题行；扫描 / 解析工具只在切到这一段时可见 -->
    <template v-if="view === 'place'">
      <section class="rounded-card border border-line bg-surface p-4 shadow-card">
        <div class="mb-3 flex flex-wrap items-center gap-2">
          <h2 class="text-sm font-medium">地点相册</h2>
          <span class="text-[11px] text-ink-2">
            {{ places.length }} 个 GPS 点位 · {{ groups.length }} 个{{ settings.level === "province" ? "省" : settings.level === "city" ? "市" : "县" }}级相册 · 详细地址 {{ addressCount }} 条
          </span>
        </div>
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
    </template>

    <!-- 地图段（§36）：点标记只联动高亮，选中后给明确切段入口，不自动跳走打断浏览 -->
    <template v-if="view === 'map'">
      <section class="rounded-card border border-line bg-surface p-4 shadow-card">
        <div class="mb-3 flex flex-wrap items-center gap-2">
          <h2 class="text-sm font-medium">地点地图</h2>
          <span class="text-[11px] text-ink-2">{{ groups.length }} 个相册 · {{ places.length }} 个点位</span>
          <button
            v-if="selectedPlace"
            type="button"
            class="ml-auto flex h-7 items-center gap-1 rounded-lg border border-line bg-surface px-2.5 text-xs text-ink-2 transition-colors hover:bg-hover hover:text-ink"
            :title="`切到地点段查看「${selectedPlace.label}」的相册卡片`"
            @click="view = 'place'"
          ><ImageIcon class="size-3.5" /> 在地点相册中查看</button>
        </div>
        <MapView :groups="groups" :selected-key="selectedKey" @select="selectGroup" />
      </section>
    </template>

    <p class="text-xs text-ink-3">
      人物与地点相册索引：<span class="break-all">{{ dbPath || "—" }}</span>（独立于评分数据库；删除会丢失人物关联，但不会改动任何照片文件）
    </p>
    </div>
  </div>
</template>
