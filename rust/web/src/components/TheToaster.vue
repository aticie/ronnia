<script setup lang="ts">
import { ref, onUnmounted } from "vue";
import { useBus, Notification } from "../composables/useBus";
import IconDone from "./icons/IconDone.vue";
import IconError from "./icons/IconError.vue";

const notifications = ref<Notification[]>([]);
const bus = useBus();

const unsub = bus.on((notification) => {
  if (!notification.id) {
    notification.id = Math.random();
  }

  setTimeout(() => {
    const index = notifications.value.findIndex((x) => x.id === notification.id);
    if (index !== -1) notifications.value.splice(index, 1);
  }, 5000);

  notifications.value.push(notification);
});

onUnmounted(unsub);
</script>

<template>
  <TransitionGroup
    tag="div"
    class="max-w-xs w-full fixed right-3 top-3 grid gap-2 z-10 text-sm"
    enter-from-class="-translate-y-full"
    leave-to-class="scale-0 opacity-0"
    enter-active-class="transition-all"
    leave-active-class="transition-all absolute"
    move-class="transition-all"
  >
    <div
      v-for="notif in notifications"
      :key="notif.id"
      role="status"
      class="flex gap-3 bg-neutral-950 border border-neutral-800 rounded-lg p-3"
    >
      <div
        class="h-7 w-7 aspect-square p-1 bg-neutral-900 rounded-full flex items-center"
        :class="{ '!bg-red-500/25': notif.type === 'error' }"
      >
        <IconDone v-if="notif.type === 'success'" />
        <IconError v-if="notif.type === 'error'" :class="{ '!fill-red-400': notif.type === 'error' }" />
      </div>

      <div class="pt-0.5 w-full">
        <p :class="{ '!text-red-400': notif.type === 'error' }">{{ notif.title }}</p>
        <p v-if="notif.description" class="text-xs mt-1 text-neutral-400">
          {{ notif.description }}
        </p>
      </div>
    </div>
  </TransitionGroup>
</template>
