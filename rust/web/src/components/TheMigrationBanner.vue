<script setup lang="ts">
import { computed } from "vue";
import { RouterLink } from "vue-router";
import { formatLocalDateTime } from "../utils";

const props = defineProps<{
  cutoffAt: string | null;
}>();

const formattedCutoff = computed(() =>
  props.cutoffAt ? formatLocalDateTime(props.cutoffAt) : undefined
);
</script>

<template>
  <section
    role="status"
    aria-live="polite"
    aria-atomic="true"
    data-testid="migration-banner"
    class="sticky top-0 z-50 w-full border-b border-rose-900/70 bg-rose-950/60 backdrop-blur"
  >
    <div
      class="mx-auto flex w-full max-w-2xl flex-col gap-3 px-4 py-3 text-sm sm:flex-row sm:items-center"
    >
      <div class="min-w-0 flex-1">
        <p class="font-semibold text-neutral-100">Ronnia is migrating</p>
        <p class="mt-1 text-neutral-300">
          All accounts and settings now use the new dashboard. To keep Ronnia
          active in your Twitch channel, log in and re-authorize Twitch bot
          access.
        </p>
        <p v-if="formattedCutoff" class="mt-1 text-neutral-400">
          Migration cutoff:
          <time :datetime="cutoffAt ?? undefined">{{ formattedCutoff }}</time>
          (your local time).
        </p>
      </div>

      <RouterLink
        to="/settings"
        class="shrink-0 rounded bg-rose-800 px-3 py-2 text-center font-medium text-neutral-100 transition-colors hover:bg-rose-700 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-rose-400"
      >
        Open settings
      </RouterLink>
    </div>
  </section>
</template>
