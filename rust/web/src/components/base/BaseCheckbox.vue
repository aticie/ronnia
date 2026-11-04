<script setup lang="ts">
defineProps<{
  modelValue?: any
}>();
defineEmits(["update:modelValue"]);
</script>

<template>
  <input
    class="appearance-none w-5 h-5 bg-neutral-950 rounded-md checked:bg-rose-800 checked:bg-[url('/tick.svg')]"
    type="checkbox"
    :value="modelValue"
    :checked="modelValue"
    @input="$emit('update:modelValue', ($event.target as HTMLInputElement).checked)"
  />
</template>
