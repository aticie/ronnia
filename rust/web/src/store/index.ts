import { ref } from "vue";
import { defineStore } from "pinia";
import type { UserDetails } from "../types";
import { api, apiErrorMessage, isUnauthorized } from "../api";

type SignupProvider = "osu" | "twitch";

export const useUserStore = defineStore("user", () => {
  const user = ref<UserDetails>();
  const signup = ref<SignupProvider>();
  const loaded = ref(false);
  const error = ref("");
  let pending: Promise<void> | undefined;

  const resetSession = (message = "") => {
    user.value = undefined;
    signup.value = undefined;
    loaded.value = true;
    error.value = message;
  };

  const getUser = (): Promise<void> => {
    if (pending) return pending;
    if (loaded.value) return Promise.resolve();
    pending = (async () => {
      error.value = "";
      try {
        const response = await api.get<UserDetails | { signup: SignupProvider } | null>("/user/me");
        user.value = undefined;
        signup.value = undefined;
        if (response.data && "signup" in response.data) {
          signup.value = response.data.signup;
        } else if (response.data) {
          user.value = response.data;
        }
        loaded.value = true;
      } catch (cause) {
        error.value = apiErrorMessage(cause);
        if (isUnauthorized(cause)) resetSession(error.value);
        else throw cause;
      } finally {
        pending = undefined;
      }
    })();
    return pending;
  };

  const retryUser = () => {
    loaded.value = false;
    return getUser();
  };

  // Axios follows the API's 303 back to the public origin; cookies are handled
  // by the browser. Only clear local state after a successful response.
  const logoutUser = async () => {
    await api.get("/user/logout");
    resetSession();
  };

  const deleteUser = async () => {
    await api.delete("/user/me");
    resetSession();
  };

  return { user, signup, loaded, error, getUser, retryUser, resetSession, logoutUser, deleteUser };
});
