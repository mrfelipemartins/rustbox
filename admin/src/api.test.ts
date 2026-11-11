// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { api, setCsrf, ApiError } from "./api";
afterEach(() => {
  vi.unstubAllGlobals();
  setCsrf("");
});
describe("admin session requests", () => {
  it("sends CSRF on mutations and uses same-origin cookies without storing the API key", async () => {
    const fetch = vi
      .fn()
      .mockResolvedValue(new Response("{}", { status: 200 }));
    vi.stubGlobal("fetch", fetch);
    setCsrf("session-csrf");
    await api("/v1/accounts/a/pause", { method: "POST" });
    const options = fetch.mock.calls[0][1];
    expect(new Headers(options.headers).get("x-csrf-token")).toBe(
      "session-csrf",
    );
    expect(new Headers(options.headers).has("Authorization")).toBe(false);
    expect(options.credentials).toBe("same-origin");
    expect(localStorage.length).toBe(0);
  });
  it("signals session expiration and retains the backend error message", async () => {
    const expired = vi.fn();
    window.addEventListener("session-expired", expired, { once: true });
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(
        new Response(JSON.stringify({ message: "Session expired" }), {
          status: 401,
        }),
      ),
    );
    await expect(api("/overview")).rejects.toMatchObject({
      status: 401,
      message: "Session expired",
    } as ApiError);
    expect(expired).toHaveBeenCalledOnce();
  });
  it("does not turn rejected login into an expiration event", async () => {
    const expired = vi.fn();
    window.addEventListener("session-expired", expired, { once: true });
    vi.stubGlobal(
      "fetch",
      vi
        .fn()
        .mockResolvedValue(
          new Response('{"message":"Invalid key"}', { status: 401 }),
        ),
    );
    await expect(
      api("/session", { method: "POST", body: '{"api_key":"invalid"}' }),
    ).rejects.toBeInstanceOf(ApiError);
    expect(expired).not.toHaveBeenCalled();
    window.removeEventListener("session-expired", expired);
  });
});
