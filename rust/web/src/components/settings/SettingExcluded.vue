<script setup lang="ts">
import { ref } from "vue";
import { useVModel } from "@vueuse/core";
import { useBus } from "../../composables";
import { api, apiErrorMessage } from "../../api";

import SettingBase from './SettingBase.vue';
import BaseButton from "../base/BaseButton.vue";
import IconDelete from "../icons/IconDelete.vue";
import IconAdd from "../icons/IconAdd.vue";

const props = defineProps<{
  modelValue: string[]
}>();
const emit = defineEmits<{
  (event: "update:modelValue", payload: string[]): void
}>();

const bus = useBus();
const users = useVModel(props, "modelValue", emit);
const user = ref("");
const loadError = ref("");
const mutationError = ref("");
const isFetching = ref(false);

const loadUsers = async () => {
  loadError.value = "";
  try {
    const response = await api.get<string[]>("/user/exclude");
    users.value = response.data;
  } catch (error) {
    loadError.value = apiErrorMessage(error);
    bus.emit({ title: "Could not load excluded users", description: loadError.value, type: "error" });
  }
};
await loadUsers();

const showMutationError = (error: unknown) => {
  mutationError.value = apiErrorMessage(error);
  bus.emit({ title: "Could not update excluded users", description: mutationError.value, type: "error" });
};

const addExcluded = async () => {
  const login = user.value.trim().toLowerCase();
  if (!login || isFetching.value || loadError.value) return;
  mutationError.value = "";
  if (users.value.includes(login)) {
    showMutationError(new Error("User already exists."));
    return;
  }
  isFetching.value = true;
  try {
    const response = await api.post<string>("/user/exclude", null, {
      params: { excluded_user: login }
    });
    // Use the API's normalized login, not the unvalidated input.
    if (!users.value.includes(response.data)) users.value.unshift(response.data);
    user.value = "";
    bus.emit({ title: `Added ${response.data} to excluded users!`, type: "success" });
  } catch (error) {
    showMutationError(error);
  } finally {
    isFetching.value = false;
  }
};

const removeExcluded = async (login: string) => {
  if (isFetching.value) return;
  mutationError.value = "";
  isFetching.value = true;
  try {
    await api.delete("/user/exclude", { params: { excluded_user: login } });
    users.value = users.value.filter((value) => value !== login);
    bus.emit({ title: `Removed ${login} from excluded users!`, type: "success" });
  } catch (error) {
    showMutationError(error);
  } finally {
    isFetching.value = false;
  }
};
</script>

<template>
  <SettingBase class="flex-col gap-2">
    <p class="text-neutral-500 font-bold text-left w-full text-sm ml-2">
      User to be excluded
    </p>


    <div v-if="loadError" role="alert" class="text-red-400 text-sm">
      <p>{{ loadError }}</p>
      <button class="underline" @click="loadUsers">Retry loading excluded users</button>
    </div>
    <p v-if="mutationError" role="alert" class="text-red-400 text-sm">{{ mutationError }}</p>
    <div class="flex flex-wrap gap-2 w-full">
      <input
        v-model="user"
        aria-label="Twitch user to exclude"
        :disabled="isFetching || !!loadError"
        @keyup.enter="addExcluded"
        class="bg-neutral-950 rounded p-2 grow"
      />
      <BaseButton @click="addExcluded" :disabled="isFetching || !!loadError" aria-label="Add excluded user" class="px-2">
        <template #icon>
          <IconAdd />
        </template>
      </BaseButton>
    </div>

    <p class="text-neutral-500 font-bold text-left w-full text-sm ml-2 mt-2">
      Users
    </p>
    <ul
      class="bg-neutral-950 max-h-60 overflow-y-auto w-full relative rounded divide-y divide-neutral-800 overflow-hidden"
    >
      <p
        v-if="!loadError && users.length === 0"
        class="p-2 text-center text-neutral-700 text-sm"
      >
        You can add users that should be excluded here!
      </p>
      <li
        v-for="user in users"
        :key="user"
        class="flex items-center justify-between p-1 group"
      >
        <p class="ml-2">{{ user }}</p>
        <BaseButton @click="removeExcluded(user)" :disabled="isFetching" :aria-label="`Remove ${user} from excluded users`" class="px-2">
          <template #icon>
            <IconDelete />
          </template>
        </BaseButton>
      </li>
    </ul>
  </SettingBase>
</template>
