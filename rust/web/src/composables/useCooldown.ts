import { ref } from "vue";

export const useCooldown = (delay: number) => {
  const count = ref(delay);
  const onCooldown = ref(false);

  let interval: number;
  const resetCooldown = () => {
    count.value = delay;
    onCooldown.value = true;

    if (interval) {
      clearInterval(interval);
    }

    interval = window.setInterval(() => {
      count.value--;
      if (count.value === 0) {
        onCooldown.value = false;
        clearInterval(interval);
      }
    }, 1000);
  }

  return {
    resetCooldown,
    onCooldown,
    count
  }
}
