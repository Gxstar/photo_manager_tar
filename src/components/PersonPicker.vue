<script setup>
import { computed, nextTick, ref, watch } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { Check, Plus, RefreshCw, TriangleAlert, Users, X } from "lucide-vue-next";
import { errMsg } from "../store.js";

/**
 * 「关联到人物」弹窗（docs §35）。
 * 图库选中照片后打开：选一个人物 / 现场新建，批量关联。
 * 空数据、重名、重复关联、读取失败都有明确出口，不把失败伪装成空态。
 */
const props = defineProps({
  open: { type: Boolean, default: false },
  /** 待关联的照片绝对路径 */
  paths: { type: Array, default: () => [] },
});
const emit = defineEmits(["close", "linked"]);

const people = ref([]);
const loading = ref(false);
const loadError = ref("");
const selectedId = ref(null);
const creating = ref(false);
const newName = ref("");
const createError = ref("");
const createBusy = ref(false);
const nameInput = ref(null);
const busy = ref(false);
const actionError = ref("");
const result = ref(null);

const canSubmit = computed(
  () => !!selectedId.value && props.paths.length > 0 && !busy.value && !result.value,
);

const resultText = computed(() => {
  const r = result.value;
  if (!r) return "";
  const parts = [];
  if (r.added) parts.push(`新增关联 ${r.added} 张`);
  if (r.duplicates) parts.push(`${r.duplicates} 张已在该人物中，已跳过`);
  if (r.skipped) parts.push(`${r.skipped} 张文件已不在库中，已跳过`);
  if (!parts.length) parts.push("没有新增关联（所选照片都已关联，或文件已不在库中）");
  return parts.join(" · ");
});

async function load() {
  loading.value = true;
  loadError.value = "";
  try {
    const list = await invoke("smart_people");
    people.value = list || [];
    // 选中项被别处删掉时清掉，避免对着一个不存在的人物提交
    if (selectedId.value && !people.value.some((p) => p.id === selectedId.value)) {
      selectedId.value = null;
    }
    if (!selectedId.value && people.value.length) selectedId.value = people.value[0].id;
  } catch (e) {
    people.value = [];
    loadError.value = errMsg(e);
  } finally {
    loading.value = false;
  }
}

function startCreate() {
  creating.value = true;
  newName.value = "";
  createError.value = "";
  actionError.value = "";
  result.value = null;
  nextTick(() => nameInput.value?.focus());
}

async function createPerson() {
  if (createBusy.value) return;
  const name = newName.value.trim();
  if (!name) {
    createError.value = "人物名字不能为空";
    return;
  }
  createBusy.value = true;
  createError.value = "";
  try {
    const p = await invoke("smart_create_person", { name });
    people.value.push(p);
    selectedId.value = p.id;
    creating.value = false;
    newName.value = "";
  } catch (e) {
    createError.value = errMsg(e);
  } finally {
    createBusy.value = false;
  }
}

async function submit() {
  if (!canSubmit.value) return;
  busy.value = true;
  actionError.value = "";
  result.value = null;
  try {
    const report = await invoke("smart_person_link", {
      id: selectedId.value,
      paths: props.paths,
    });
    result.value = report;
    const person = people.value.find((p) => p.id === selectedId.value);
    emit("linked", { id: selectedId.value, name: person?.name ?? "", report });
  } catch (e) {
    actionError.value = errMsg(e);
  } finally {
    busy.value = false;
  }
}

function close() {
  emit("close");
}

watch(
  () => props.open,
  (v) => {
    if (!v) return;
    // 每次打开都是一次全新的动作：清掉上次的结果与错误，再拉最新人物列表
    result.value = null;
    actionError.value = "";
    createError.value = "";
    creating.value = false;
    newName.value = "";
    load();
  },
);
</script>

<template>
  <Teleport to="body">
    <div
      v-if="open"
      class="fixed inset-0 z-50 grid place-items-center px-4"
      style="background-color: rgb(46 42 36 / 0.30)"
      @click.self="close"
    >
      <div class="flex max-h-[80vh] w-full max-w-md flex-col overflow-hidden rounded-card border border-line bg-surface shadow-card">
        <header class="flex items-start justify-between gap-3 border-b border-line px-5 py-3">
          <div class="min-w-0">
            <h2 class="flex items-center gap-1.5 text-sm font-semibold"><Users class="size-4" /> 关联到人物</h2>
            <p class="mt-0.5 text-[11px] leading-4 text-ink-2">
              已选 {{ paths.length }} 张照片 · 只写本地索引，不改动照片文件
            </p>
          </div>
          <button
            class="grid size-7 shrink-0 place-items-center rounded-lg text-ink-2 transition-colors hover:bg-hover"
            aria-label="关闭"
            @click="close"
          >
            <X class="size-4" />
          </button>
        </header>

        <div class="min-h-0 flex-1 overflow-y-auto px-4 py-3">
          <div v-if="loading" class="py-12 text-center text-xs text-ink-2">读取人物…</div>

          <div v-else-if="loadError" class="space-y-2 py-8">
            <p class="flex items-start gap-1.5 rounded-lg bg-bad-soft px-3 py-2 text-xs leading-5 text-bad">
              <TriangleAlert class="mt-0.5 size-3.5 shrink-0" /> 读取人物失败：{{ loadError }}
            </p>
            <button
              class="mx-auto flex h-8 items-center gap-1.5 rounded-lg border border-line px-3 text-xs text-ink-2 transition-colors hover:bg-hover"
              @click="load"
            >
              <RefreshCw class="size-3.5" /> 重试
            </button>
          </div>

          <template v-else>
            <div v-if="!people.length && !creating" class="py-10 text-center">
              <p class="text-xs leading-5 text-ink-2">还没有人物。先建一个，再把选中的照片关联进去。</p>
            </div>

            <div v-if="people.length" class="space-y-1">
              <label
                v-for="p in people"
                :key="p.id"
                class="flex cursor-pointer items-center gap-3 rounded-lg border px-3 py-2 transition-colors"
                :class="selectedId === p.id ? 'border-amber bg-amber-soft' : 'border-line hover:bg-hover'"
              >
                <input
                  type="radio"
                  class="size-3.5 shrink-0"
                  style="accent-color: var(--color-amber)"
                  :checked="selectedId === p.id"
                  @change="selectedId = p.id"
                />
                <span class="min-w-0 flex-1 truncate text-xs font-medium" :title="p.name">{{ p.name }}</span>
                <span class="shrink-0 text-[11px] text-ink-2 tabular-nums">{{ p.count }} 张</span>
              </label>
            </div>

            <div v-if="creating" class="mt-2 flex items-center gap-2">
              <input
                v-model="newName"
                class="h-8 min-w-0 flex-1 rounded-lg border border-line bg-subtle px-2.5 text-xs outline-none focus:border-amber"
                maxlength="40"
                ref="nameInput"
                placeholder="人物名字，如 阿星"
                @keyup.enter="createPerson"
                @keyup.esc.stop="creating = false; createError = ''"
              />
              <button
                class="h-8 shrink-0 rounded-lg bg-amber px-3 text-xs font-medium text-ink ring-1 ring-inset ring-amber-deep/45 transition-colors hover:bg-amber-deep disabled:opacity-50"
                :disabled="createBusy"
                @click="createPerson"
              >{{ createBusy ? "创建中…" : "创建" }}</button>
              <button
                class="h-8 shrink-0 rounded-lg border border-line px-2.5 text-xs text-ink-2 hover:bg-hover"
                @click="creating = false; createError = ''"
              >取消</button>
            </div>
            <button
              v-else
              class="mt-2 flex h-8 w-full items-center justify-center gap-1.5 rounded-lg border border-dashed border-line-strong text-xs text-ink-2 transition-colors hover:bg-hover hover:text-ink"
              @click="startCreate"
            >
              <Plus class="size-3.5" /> 新建人物
            </button>

            <p v-if="createError" class="mt-2 flex items-center gap-1.5 rounded-md bg-bad-soft px-2.5 py-1.5 text-[11px] text-bad">
              <TriangleAlert class="size-3.5 shrink-0" /> {{ createError }}
            </p>
          </template>
        </div>

        <footer class="border-t border-line px-5 py-3">
          <p v-if="actionError" class="mb-2 flex items-start gap-1.5 rounded-md bg-bad-soft px-2.5 py-1.5 text-[11px] leading-4 text-bad">
            <TriangleAlert class="mt-0.5 size-3.5 shrink-0" /> {{ actionError }}
          </p>
          <p v-if="result" class="mb-2 flex items-start gap-1.5 rounded-md px-2.5 py-1.5 text-[11px] leading-4" :class="result.added ? 'bg-ok-soft text-ok' : 'bg-info-soft text-info'">
            <Check class="mt-0.5 size-3.5 shrink-0" /> {{ resultText }}
          </p>
          <div class="flex items-center justify-between gap-3">
            <span class="text-xs text-ink-2">{{ people.length }} 个人物 · 已选 {{ paths.length }} 张</span>
            <div class="flex shrink-0 items-center gap-2">
              <button
                class="rounded-lg border border-line px-3 py-1.5 text-xs text-ink-2 transition-colors hover:bg-hover"
                @click="close"
              >{{ result ? "完成" : "取消" }}</button>
              <button
                class="rounded-lg bg-amber px-3 py-1.5 text-xs font-medium text-ink ring-1 ring-inset ring-amber-deep/45 transition-opacity hover:bg-amber-deep disabled:opacity-40"
                :disabled="!canSubmit || !!loadError"
                @click="submit"
              >{{ busy ? "关联中…" : `关联 ${paths.length} 张` }}</button>
            </div>
          </div>
        </footer>
      </div>
    </div>
  </Teleport>
</template>
