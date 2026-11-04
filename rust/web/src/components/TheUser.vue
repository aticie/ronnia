<script setup lang="ts">
import { useRouter } from "vue-router";
import { useUserStore } from "../store";
import { ref } from "vue";
import { apiErrorMessage, twitchLogin } from "../api";
import { useBus } from "../composables";

import IconOsu from "./icons/IconOsu.vue";
import IconTwitch from "./icons/IconTwitch.vue";
import IconLogout from "./icons/IconLogout.vue";
import IconDelete from "./icons/IconDelete.vue";
import BaseButton from "./base/BaseButton.vue";

const userStore = useUserStore();
const router = useRouter();

const bus = useBus();
const isFetching = ref(false);

const accountAction = async (action: () => Promise<void>) => {
  if (isFetching.value) return;
  isFetching.value = true;
  try {
    await action();
    await router.replace("/login");
  } catch (error) {
    bus.emit({ title: "Account action failed", description: apiErrorMessage(error), type: "error" });
  } finally {
    isFetching.value = false;
  }
};
const removeUser = () => accountAction(userStore.deleteUser);
const logout = () => accountAction(userStore.logoutUser);
</script>

<template>
  <div v-if="userStore.user" class="grid gap-2">
    <div class="flex gap-2">
      <div class="bg-neutral-900 p-2 rounded flex-1 relative">
        <IconOsu class="h-6 mb-2 absolute right-2 opacity-40" />
        <div class="flex items-center gap-2">
          <img
            :src="userStore.user.osuAvatarUrl"
            class="w-14 aspect-square rounded"
            alt="osu user avatar"
          />
          <p>{{ userStore.user.osuUsername }}</p>
        </div>
      </div>

      <div
        class="bg-neutral-900 p-2 rounded flex-1 relative"
        :class="{ 'outline outline-2 outline-green-500': userStore.user.isLive }"
      >
        <IconTwitch class="h-6 absolute right-2 opacity-40" />
        <div class="flex items-center gap-2">
          <img
            :src="userStore.user.twitchAvatarUrl"
            class="w-14 aspect-square rounded"
            alt="twitch user avatar"
          />
          <p>{{ userStore.user.twitchUsername }}</p>
        </div>

        <div class="flex items-center gap-2 mt-2">
          <div
            class="w-2.5 h-2.5 bg-neutral-500 rounded-full"
            :class="{ '!bg-green-500': userStore.user.isLive }"
          />
          <p class="text-sm">
            {{ userStore.user.isLive ? "Online" : "Offline" }}
          </p>
        </div>
      </div>
    </div>

    <div v-if="!userStore.user.twitchBotAuthorized" role="status" class="rounded bg-neutral-900 p-3 text-sm">
      <p class="font-semibold">Twitch bot consent is required</p>
      <p class="mt-1 text-neutral-400">
        Authorize Ronnia to act as a bot in your channel. Sign in as
        {{ userStore.user.twitchUsername }}: only this linked Twitch account can
        renew consent. Your linked osu! account and settings will be kept.
      </p>
      <a :href="twitchLogin" class="inline-block mt-3 rounded bg-rose-800 px-3 py-2">
        Re-authorize Twitch
      </a>
    </div>

    <div class="flex gap-2">
      <BaseButton @click="logout" :disabled="isFetching" class="basis-44">
        <template #icon>
          <IconLogout />
        </template>
        <p>Logout</p>
      </BaseButton>

      <BaseButton @click="removeUser" :disabled="isFetching" class="basis-44">
        <template #icon>
          <IconDelete />
        </template>
        <p>Delete Account</p>
      </BaseButton>
    </div>
  </div>
</template>
