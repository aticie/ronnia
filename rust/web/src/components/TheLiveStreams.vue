<script setup lang="ts">
import { api, apiErrorMessage } from "../api";
import { ref, onMounted } from "vue";

const streamers = ref<string[]>([]);
const loadError = ref("");
const target = ref<HTMLElement | null>(null);
let rowCount = 0;

try {
  const response = await api.get<string[]>("/live/users", { params: { limit: 100 } });
  const users = response.data;
  document.documentElement.style.setProperty("--duration", `${Math.max(users.length, 1) * 1.38}s`);
  // Keep short lists visible too; the upstream splice erased 1–3 live users.
  rowCount = Math.ceil(users.length / 4);
  streamers.value = [...users, ...users];
} catch (error) {
  loadError.value = apiErrorMessage(error);
}

onMounted(() => {
  target.value?.style.setProperty("--row", `${-209 * rowCount}px`);
});
</script>

<template>
  <div class="absolute flex items-end bottom-0 inset-x-0 w-full pt-10 h-[70%] overflow-hidden">
    <div class="surface mx-auto">
      <p
        class="text-neutral-400 text-center text-3xl select-none"
        style="font-family: 'Bebas Neue', cursive"
      >
        Some cool streamers that use Ronnia!
      </p>
      <p v-if="loadError" role="alert" class="text-red-400 text-center text-sm">
        Could not load live streams: {{ loadError }}
      </p>
      <p v-else-if="!streamers.length" class="text-neutral-500 text-center text-sm">
        No Ronnia streamers are live right now.
      </p>

      <div
        class="mask h-[612px] overflow-hidden flex flex-col items-center -z-10"
      >
        <div
          ref="target"
          class="animate-scroll max-w-7xl grid grid-cols-4 gap-2"
        >
          <a
            v-for="(streamer, index) in streamers"
            :key="`${streamer}-${index}`"
            :href="`https://twitch.tv/${streamer}`"
            class="group"
          >
            <div class="group-hover:bg-rose-700 transition-colors rounded-xl">
              <img
                :src="`https://static-cdn.jtvnw.net/previews-ttv/live_user_${streamer}-484x268.jpg`"
                class="border border-neutral-700 rounded-lg aspect-video object-cover group-hover:-translate-x-4 group-hover:-translate-y-4 transition-transform"
                alt="twitch live stream"
              />
            </div>
            <p class="ml-4" style="font-family: 'Bebas Neue', cursive">
              {{ streamer }}
            </p>
          </a>
        </div>
      </div>
    </div>
  </div>
</template>
