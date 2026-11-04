import { useEventBus } from "@vueuse/core";

export interface Notification {
  id?: number,
  title: string,
  description?: string,
  type?: "error" | "success"
}

export const useBus = () => {
  const bus = useEventBus<Notification>("notification");
  return bus;
}
