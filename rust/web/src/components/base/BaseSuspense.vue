<script setup lang="ts">
import IconSpinner from '../icons/IconSpinner.vue';
</script>

<template>
  <Suspense :timeout="0">
    <template #default>
      <slot></slot>
    </template>

    <template #fallback>
      <IconSpinner class="mx-auto" />
    </template>
  </Suspense>
</template>
