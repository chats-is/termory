import { beforeEach, describe, expect, it, vi } from "vitest";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invoke(...args) }));

describe("gateways cache", () => {
  beforeEach(() => {
    vi.resetModules();
    invoke.mockReset();
  });

  it("re-reads gateways only after invalidateGatewaysCache", async () => {
    const config = await import("./config");
    invoke.mockResolvedValueOnce([{ id: "a" }]);
    expect(await config.getConfig("gateways")).toEqual([{ id: "a" }]);

    // The backend wrote providers.json (router entry synced) behind the cache.
    invoke.mockResolvedValue([{ id: "a" }, { id: "router" }]);
    config.invalidateConfigCache(); // config.json only — gateways stay cached
    expect(await config.getConfig("gateways")).toEqual([{ id: "a" }]);

    config.invalidateGatewaysCache();
    expect(await config.getConfig("gateways")).toEqual([{ id: "a" }, { id: "router" }]);
    expect(invoke.mock.calls.filter((c) => c[0] === "read_app_gateways")).toHaveLength(2);
  });
});
