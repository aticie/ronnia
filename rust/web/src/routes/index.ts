import { createRouter, createWebHistory } from "vue-router";

import Settings from "../views/Settings.vue";
import Login from "../views/Login.vue";
import { useUserStore } from "../store";

const router = createRouter({
  history: createWebHistory(),
  routes: [
    {
      path: "/",
      redirect: "/login"
    },
    {
      path: "/login",
      component: Login
    },
    {
      path: "/settings",
      component: Settings,
      beforeEnter: async () => {
        const userStore = useUserStore();
        try {
          await userStore.getUser();
        } catch {
          // getUser retains the API error for the login status and retry action.
          return { path: "/login" };
        }
        return userStore.user ? true : { path: "/login" };
      }
    }
  ]
});

export default router;
