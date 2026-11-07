<script setup>
import { onMounted, ref } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { Check, ChevronDown, Trash2, TriangleAlert } from "lucide-vue-next";
import SettingRow from "./SettingRow.vue";

/** 设置页「智能相册 · 在线地点」行（docs §17.13）：key 只存本机 smart.db，不上传任何照片信息。 */
const keyInput = ref("");
const keySet = ref(false);
const level = ref("city");
const notice = ref("");
const error = ref("");

const levels = [
  { id: "province", label: "省" },
  { id: "city", label: "市" },
  { id: "county", label: "县" },
];

async function load() {
  try {
    const s = await invoke("smart_get_settings");
    keySet.value = s.keySet;
    level.value = s.level;
    keyInput.value = "";
  } catch (e) {
    error.value = String(e);
  }
}

async function saveKey() {
  error.value = "";
  notice.value = "";
  try {
    await invoke("smart_set_tianditu_key", { key: keyInput.value });
    await load();
    notice.value = keySet.value ? "已保存，可到智能相册点「解析地名」" : "已清除";
  } catch (e) {
    error.value = String(e);
  }
}

async function clearKey() {
  keyInput.value = "";
  await saveKey();
}

async function setLevel(v) {
  level.value = v;
  error.value = "";
  notice.value = "";
  try {
    await invoke("smart_set_place_level", { level: v });
  } catch (e) {
    error.value = String(e);
  }
}

onMounted(load);
</script>

<template>
  <SettingRow title="天地图 key" desc="只把坐标解析成地名；只发送经纬度">
    <template #title>
      天地图 key
      <span
        class="inline-flex h-5 items-center rounded-full px-1.5 text-[11px] font-normal"
        :class="keySet ? 'bg-ok-soft text-ok' : 'bg-subtle text-ink-2'"
      >{{ keySet ? "已设置" : "未设置" }}</span>
    </template>
    <input
      v-model="keyInput"
      type="password"
      autocomplete="off"
      class="h-8 w-44 min-w-0 rounded-lg border border-line bg-subtle px-2.5 text-xs outline-none focus:border-amber"
      :placeholder="keySet ? '留空并保存可清除' : '服务端类型 key'"
    />
    <button
      class="inline-flex h-8 shrink-0 items-center gap-1.5 rounded-lg bg-amber px-3 text-xs font-medium text-ink ring-1 ring-inset ring-amber-deep/45 transition-colors hover:bg-amber-deep"
      @click="saveKey"
    >
      保存
    </button>
    <button
      v-if="keySet"
      class="grid size-8 shrink-0 place-items-center rounded-lg border border-line text-ink-2 transition-colors hover:bg-hover"
      title="清除 key"
      aria-label="清除 key"
      @click="clearKey"
    >
      <Trash2 class="size-3.5" />
    </button>
  </SettingRow>

  <SettingRow title="相册名称显示到" desc="未解析的相册回退显示坐标">
    <span class="inline-flex h-8 overflow-hidden rounded-lg border border-line text-xs">
      <button
        v-for="l in levels"
        :key="l.id"
        type="button"
        class="px-3 transition-colors"
        :class="level === l.id ? 'bg-amber-soft font-medium text-amber-text' : 'text-ink-2 hover:bg-hover'"
        @click="setLevel(l.id)"
      >{{ l.label }}</button>
    </span>
  </SettingRow>

  <p v-if="notice" class="flex items-center gap-1.5 bg-ok-soft px-5 py-2.5 text-xs text-ok">
    <Check class="size-3.5 shrink-0" /> {{ notice }}
  </p>
  <p v-if="error" class="flex items-center gap-1.5 bg-bad-soft px-5 py-2.5 text-xs text-bad">
    <TriangleAlert class="size-3.5 shrink-0" /> {{ error }}
  </p>

  <details class="group">
    <summary
      class="flex cursor-pointer list-none items-center gap-4 px-5 py-2.5 text-[11px] text-ink-3 transition-colors hover:bg-hover/60"
    >
      <span class="flex-1">关于 key 类型、配额与联网时机</span>
      <ChevronDown class="chev size-3.5 transition-transform duration-150" />
    </summary>
    <p class="px-5 pb-4 text-[11px] leading-5 text-ink-2">
      桌面应用请申请<strong class="font-medium text-ink">服务端</strong>类型 key。地图底图用高德公开瓦片，不需要 key。
      个人开发者逆地理编码日配额约 7000 次；按地点相册缓存结果，已解析的不会重复请求。
      解析只在你点「解析地名」时发生，默认不联网。
    </p>
  </details>
</template>

<style scoped>
details[open] .chev {
  transform: rotate(180deg);
}
</style>
