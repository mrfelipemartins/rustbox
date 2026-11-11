import { useQuery } from "@tanstack/react-query";
import type { Row } from "./lib";
export type Session = {
  application: string;
  csrf_token: string;
  expires_at: string;
};
let csrf = "";
export function setCsrf(value: string) {
  csrf = value;
}
export class ApiError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}
export async function api<T = Row>(
  path: string,
  options: RequestInit = {},
): Promise<T> {
  const headers = new Headers(options.headers);
  headers.set("Accept", "application/json");
  if (options.body) headers.set("Content-Type", "application/json");
  if (!["GET", "HEAD"].includes(options.method || "GET") && csrf)
    headers.set("x-csrf-token", csrf);
  const r = await fetch("/admin/api" + path, {
    ...options,
    headers,
    credentials: "same-origin",
  });
  if (!r.ok) {
    let body: Row = {};
    try {
      body = await r.json();
    } catch {
      /* no response body */
    }
    if (r.status === 401 && path != "/session")
      window.dispatchEvent(new Event("session-expired"));
    throw new ApiError(
      r.status,
      body.message ||
        body.error?.message ||
        textError(body.error) ||
        `Request failed (${r.status})`,
    );
  }
  if (r.status === 204) return undefined as T;
  return r.json();
}
function textError(v: unknown) {
  return typeof v === "string" ? v : "";
}
export function useApi<T = Row>(
  path: string,
  enabled = true,
  interval: number | false = 10000,
) {
  return useQuery({
    queryKey: [path],
    queryFn: () => api<T>(path),
    enabled,
    refetchInterval: interval,
    refetchIntervalInBackground: false,
    retry: 1,
  });
}
export const operation = (
  path: string,
  method = "POST",
  data?: unknown,
  headers?: HeadersInit,
) =>
  api("/v1" + path, {
    method,
    body: data === undefined ? undefined : JSON.stringify(data),
    headers,
  });
