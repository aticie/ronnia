import axios from "axios";
import type { MigrationStatus } from "./types";

// Public, same-origin paths only. Provider credentials stay on the Rust server.
export const api = axios.create({ baseURL: "/api", withCredentials: true });
export const twitchLogin = "/api/oauth2/twitch-login";
export const osuLogin = "/api/oauth2/osu-login";

export async function fetchMigrationStatus(): Promise<MigrationStatus> {
  const response = await api.get<MigrationStatus>("/migration/status");
  return response.data;
}

export const isUnauthorized = (error: unknown) =>
  axios.isAxiosError(error) && error.response?.status === 401;

export function apiErrorMessage(error: unknown): string {
  if (isUnauthorized(error)) return "Your session has expired. Please log in again.";
  if (axios.isAxiosError(error)) {
    const data: unknown = error.response?.data;
    if (typeof data === "string" && data && !data.trimStart().startsWith("<")) return data;
    if (data && typeof data === "object" && "detail" in data && typeof data.detail === "string") {
      return data.detail;
    }
    if (data && typeof data === "object" && "message" in data && typeof data.message === "string") {
      return data.message;
    }
    return error.message;
  }
  return error instanceof Error ? error.message : "An unexpected error occurred. Please try again.";
}
