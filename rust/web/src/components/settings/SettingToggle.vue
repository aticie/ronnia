<script setup lang="ts">
import SettingBase from "./SettingBase.vue";
import BaseCheckbox from '../base/BaseCheckbox.vue';
import { SettingToggle } from "../../types";

defineProps<{
  data: SettingToggle
}>();
</script>

<template>
  <SettingBase>
    <label :for="data.name" class="select-none w-full">{{ data.description }}</label>
    <BaseCheckbox :id="data.name" v-model="data.value" />
  </SettingBase>
</template>
