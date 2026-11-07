<script setup>
import { computed, onBeforeUnmount, onMounted, ref, watch, nextTick } from "vue";
import L from "leaflet";
import "leaflet/dist/leaflet.css";
import { MapPin, WifiOff } from "lucide-vue-next";

/** 全幅地图视图（docs §19.5 方案 B / §24）：左「分级相册」列表 + 右 Leaflet 地图。
 *  列表按当前省 / 市 / 县级别合并，地图标记仍按原始 GPS 簇打点，精度不丢。
 *  底图固定用高德（免 key、GCJ-02 纠偏）；曾经接过的天地图瓦片已按用户要求移除（§29），
 *  天地图 key 现在只用于「解析地名」。 */
const props = defineProps({
  groups: { type: Array, default: () => [] },
  selectedKey: { type: String, default: null },
});
const emit = defineEmits(["select"]);

const mapEl = ref(null);
const offline = ref(false);
let map = null;
let tiles = null;
let markers = null;

const clusters = computed(() =>
  props.groups.flatMap((g) =>
    g.clusters.map((c) => ({ ...c, groupKey: g.key, label: g.label })),
  ),
);

const PI = Math.PI;
const AXIS = 6378245.0;
const EE = 0.00669342162296594323;

function outOfChina(lat, lon) {
  return lon < 72.004 || lon > 137.8347 || lat < 0.8293 || lat > 55.8271;
}
function tLat(x, y) {
  let r = -100 + 2 * x + 3 * y + 0.2 * y * y + 0.1 * x * y + 0.2 * Math.sqrt(Math.abs(x));
  r += ((20 * Math.sin(6 * x * PI) + 20 * Math.sin(2 * x * PI)) * 2) / 3;
  r += ((20 * Math.sin(y * PI) + 40 * Math.sin((y / 3) * PI)) * 2) / 3;
  r += ((160 * Math.sin((y / 12) * PI) + 320 * Math.sin((y * PI) / 30)) * 2) / 3;
  return r;
}
function tLon(x, y) {
  let r = 300 + x + 2 * y + 0.1 * x * x + 0.1 * x * y + 0.1 * Math.sqrt(Math.abs(x));
  r += ((20 * Math.sin(6 * x * PI) + 20 * Math.sin(2 * x * PI)) * 2) / 3;
  r += ((20 * Math.sin(x * PI) + 40 * Math.sin((x / 3) * PI)) * 2) / 3;
  r += ((150 * Math.sin((x / 12) * PI) + 300 * Math.sin((x / 30) * PI)) * 2) / 3;
  return r;
}
/** smart.db 只存 WGS-84；高德瓦片是 GCJ-02，显示前必须转换。 */
function toDisplay(lat, lon) {
  if (outOfChina(lat, lon)) return [lat, lon];
  let dLat = tLat(lon - 105, lat - 35);
  let dLon = tLon(lon - 105, lat - 35);
  const rad = (lat / 180) * PI;
  let magic = Math.sin(rad);
  magic = 1 - EE * magic * magic;
  const sqrt = Math.sqrt(magic);
  dLat = (dLat * 180) / (((AXIS * (1 - EE)) / (magic * sqrt)) * PI);
  dLon = (dLon * 180) / ((AXIS / sqrt) * Math.cos(rad) * PI);
  return [lat + dLat, lon + dLon];
}

function tileUrl() {
  return "https://webrd0{s}.is.autonavi.com/appmaptile?lang=zh_cn&size=1&scale=1&style=8&x={x}&y={y}&z={z}";
}

function renderMarkers() {
  if (!map || !markers) return;
  markers.clearLayers();
  for (const c of clusters.value) {
    const [lat, lon] = toDisplay(c.lat, c.lon);
    const on = c.groupKey === props.selectedKey;
    const icon = L.divIcon({
      className: "",
      html: `<span style="display:block;width:${on ? 16 : 11}px;height:${on ? 16 : 11}px;border-radius:50%;background:#F59E0B;border:2px solid #fff"></span>`,
      iconSize: [on ? 16 : 11, on ? 16 : 11],
      iconAnchor: [on ? 8 : 6, on ? 8 : 6],
    });
    const m = L.marker([lat, lon], { icon }).addTo(markers);
    m.bindPopup(
      `<div style="font:12px 'Segoe UI','Microsoft YaHei',sans-serif;color:#2E2A24;min-width:150px;max-width:240px">
         <div style="font-weight:500">${escapeHtml(c.label)}</div>
         ${c.address ? `<div style="color:#2E2A24;margin:3px 0">${escapeHtml(c.address)}</div>` : ""}
         <div style="color:#7B7367;margin:2px 0 6px">${c.count} 张 · ${c.lat.toFixed(4)}°, ${c.lon.toFixed(4)}°</div>
       </div>`,
    );
    m.on("click", () => emit("select", c.groupKey));
  }
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
}

function focusSelected() {
  if (!map || !props.selectedKey) return;
  const g = props.groups.find((x) => x.key === props.selectedKey);
  const c = g?.clusters?.[0];
  if (!c) return;
  const [lat, lon] = toDisplay(c.lat, c.lon);
  map.setView([lat, lon], Math.max(map.getZoom(), 11));
}

onMounted(() => {
  map = L.map(mapEl.value, { zoomControl: false, attributionControl: false }).setView([30.25, 120.15], 5);
  L.control.zoom({ position: "topleft" }).addTo(map);
  L.control.attribution({ prefix: false, position: "bottomright" })
    .addAttribution("© 高德地图")
    .addTo(map);
  tiles = L.tileLayer(tileUrl(), { subdomains: "1234", maxZoom: 18 }).addTo(map);
  tiles.on("tileerror", () => (offline.value = true));
  markers = L.layerGroup().addTo(map);
  renderMarkers();
  if (clusters.value.length) focusSelected();
  nextTick(() => map?.invalidateSize());
});
watch(clusters, renderMarkers, { deep: true });
watch(() => props.selectedKey, focusSelected);
onBeforeUnmount(() => map?.remove());
</script>

<template>
  <div class="flex gap-3" style="min-width: 0">
    <div class="w-52 shrink-0 overflow-hidden rounded-card border border-line bg-surface">
      <div class="border-b border-line px-3 py-2 text-xs text-ink-2">地点 · {{ groups.length }} 个相册 / {{ clusters.length }} 个点位</div>
      <div class="max-h-[360px] overflow-y-auto">
        <button
          v-for="g in groups"
          :key="g.key"
          type="button"
          class="relative flex w-full items-center gap-2 border-b border-line px-3 py-2 text-left last:border-0"
          :class="g.key === selectedKey ? 'bg-amber-soft' : 'hover:bg-hover'"
          @click="emit('select', g.key)"
        >
          <span v-if="g.key === selectedKey" class="absolute left-0 top-2 bottom-2 w-[3px] rounded-full bg-amber"></span>
          <MapPin class="size-3.5 shrink-0 text-ink-3" />
          <span class="min-w-0 flex-1">
            <span class="block truncate text-xs">{{ g.label }}</span>
            <span class="block text-[11px] text-ink-2">{{ g.count }} 张 · {{ g.clusters.length }} 个点位</span>
          </span>
        </button>
        <div v-if="!groups.length" class="px-3 py-6 text-center text-xs text-ink-2">暂无带 GPS 坐标的照片</div>
      </div>
    </div>

    <div class="min-w-0 flex-1">
      <div class="mb-2 flex flex-wrap items-center gap-2">
        <span class="text-[11px] text-ink-2">底图：高德（免 key，WGS-84 → GCJ-02 已纠偏）</span>
        <span v-if="offline" class="ml-auto flex items-center gap-1 text-[11px] text-warn">
          <WifiOff class="size-3.5" /> 瓦片加载失败，仍可看左侧坐标列表
        </span>
      </div>
      <div ref="mapEl" class="h-[360px] rounded-card border border-line bg-subtle"></div>
      <div class="mt-2 flex flex-wrap gap-3 text-[11px] text-ink-2">
        <span>点标记可联动相册 · 弹窗显示详细地址</span>
        <span>无网时降级为坐标与地名，不影响浏览</span>
      </div>
    </div>
  </div>
</template>
