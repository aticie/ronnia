<script setup lang="ts">
import BaseSuspense from "../components/base/BaseSuspense.vue";
import BaseButton from "../components/base/BaseButton.vue";
import BaseIcon from "../components/base/BaseIcon.vue";
import TheLiveStreams from "../components/TheLiveStreams.vue";
import IconDiscord from "../components/icons/IconDiscord.vue";
import IconTwitch from "../components/icons/IconTwitch.vue";
import IconGithub from "../components/icons/IconGithub.vue";
import IconOsu from "../components/icons/IconOsu.vue";

import { computed } from "vue";
import { useWindowSize } from "@vueuse/core";
import { useRouter } from "vue-router";
import { useUserStore } from "../store";
import { twitchLogin, osuLogin } from "../api";

const { width } = useWindowSize();
const router = useRouter();
const userStore = useUserStore();
const signup = computed(() => userStore.signup);

const loadUser = async (retry = false) => {
  try {
    await (retry ? userStore.retryUser() : userStore.getUser());
    if (userStore.user) await router.replace("/settings");
  } catch {
    // The store retains a visible error; do not pretend authentication worked.
  }
};
await loadUser();
</script>

<template>
  <div class="w-full snap-center">
    <div class="h-screen max-w-sm mx-auto flex flex-col pt-32 gap-10">
      <div class="flex justify-center items-end -ml-8 z-10">
        <h1 class="font-bold text-center text-xl">
          <span
            class="text-rose-700 text-2xl translate-y-[0.450px] translate-x-[3px] inline-block"
            >R</span
          >
          onnia Dashboard
        </h1>
      </div>

      <div class="grid gap-2 z-10">
        <div class="ml-0.5 font-semibold">
          <p v-if="!signup">Log in with:</p>
          <p v-else>Complete your sign-up to Ronnia with</p>
        </div>
        <div v-if="userStore.error" role="alert" class="text-sm text-red-400">
          <p>{{ userStore.error }}</p>
          <button class="underline" @click="loadUser(true)">Retry loading account</button>
        </div>

        <a v-if="!signup || signup === 'osu'" :href="twitchLogin">
          <BaseButton class="w-full">
            <template #icon>
              <IconTwitch />
            </template>
            <p>Twitch</p>
          </BaseButton>
        </a>

        <a v-if="!signup || signup === 'twitch'" :href="osuLogin">
          <BaseButton class="w-full">
            <template #icon>
              <IconOsu />
            </template>
            <p>osu!</p>
          </BaseButton>
        </a>
      </div>

      <div class="flex flex-col items-start z-10">
        <p class="text-xs text-neutral-500 font-bold ml-0.5 mb-1">Links</p>
        <div
          class="flex rounded overflow-hidden bg-neutral-900 divide-x divide-neutral-950"
        >
          <a href="https://discord.gg/J4WPJsceFc">
            <BaseIcon>
              <IconDiscord />
            </BaseIcon>
          </a>
          <a href="https://github.com/aticie/ronnia-web">
            <BaseIcon>
              <IconGithub />
            </BaseIcon>
          </a>
        </div>
      </div>

      <BaseSuspense v-if="width > 1024">
        <TheLiveStreams />
      </BaseSuspense>
    </div>
  </div>
</template>
