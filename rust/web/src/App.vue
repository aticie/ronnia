<script setup lang="ts">
import { ref } from "vue";
import Development from "./components/TheDevRouter.vue";
import TheMigrationBanner from "./components/TheMigrationBanner.vue";
import TheToaster from "./components/TheToaster.vue";
import BaseSuspense from "./components/base/BaseSuspense.vue";
import { fetchMigrationStatus } from "./api";

const isDev = import.meta.env.DEV;
const migrationActive = ref(false);
const migrationCutoffAt = ref<string | null>(null);

// Public rollout status is optional configuration: a missing or failing
// endpoint must never prevent the dashboard from rendering.
const loadMigrationStatus = async () => {
  try {
    const status = await fetchMigrationStatus();
    migrationActive.value = status.active === true;
    migrationCutoffAt.value =
      typeof status.cutoffAt === "string" ? status.cutoffAt : null;
  } catch {
    migrationActive.value = false;
    migrationCutoffAt.value = null;
  }
};
void loadMigrationStatus();
</script>

<template>
  <div class="min-h-screen flex flex-col items-center">
    <TheMigrationBanner
      v-if="migrationActive"
      :cutoff-at="migrationCutoffAt"
    />
    <TheToaster />

    <RouterView v-slot="{ Component }">
      <BaseSuspense>
        <component :is="Component" />
      </BaseSuspense>
    </RouterView>

    <Development v-if="isDev" />
  </div>
</template>
