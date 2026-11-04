<script setup lang="ts">
import { useRouter, RouterLink } from "vue-router";
import BaseButton from "./base/BaseButton.vue";

const router = useRouter();
</script>

<template>
  <div class="fixed left-5 bottom-5 bg-yellow-600 rounded p-4">
    <p class="mb-4 text-yellow-200 text-sm font-bold">Development Routes</p>
    <div class="flex gap-2">
      <RouterLink v-for="route in router.options.routes" :to="route.path">
        <BaseButton class="bg-yellow-700">
          {{ route.path }}
        </BaseButton>
      </RouterLink>
    </div>
  </div>
</template>
