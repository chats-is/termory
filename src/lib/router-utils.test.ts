import { describe, expect, it } from "vitest";
import {
  canMoveCandidate,
  candidateReason,
  filterRouterModels,
  sameBindings,
  formatCountdown,
  moveCandidate,
  parsePort,
  hostMode,
  prefsFromCandidates,
  sameStoredList,
  toggleCandidate,
  upstreamState
} from "./router-utils";
import type { RouterCandidate, RouterUpstreamHealth } from "@/types";

function cand(key: string, over: Partial<RouterCandidate> = {}): RouterCandidate {
  return {
    key,
    kind: "provider",
    app: "claude",
    label: key,
    detail: "",
    protocols: ["anthropic"],
    available: true,
    reason: null,
    reasonCode: null,
    enabled: true,
    ...over
  };
}

function health(over: Partial<RouterUpstreamHealth> = {}): RouterUpstreamHealth {
  return {
    key: "a",
    requests: 0,
    failures: 0,
    streak: 0,
    lastError: null,
    cooldownUntil: null,
    lastUsedAt: null,
    ...over
  };
}

describe("sameBindings", () => {
  const claude = { id: "a", app: "claude", model: "m" };
  const codex = { id: "b", app: "codex" };
  const gemini = { id: "c", app: "gemini" };
  it("ignores order", () => {
    expect(sameBindings([claude, gemini, codex], [claude, codex, gemini])).toBe(true);
  });
  it("sees a real change", () => {
    expect(sameBindings([claude, codex], [claude])).toBe(false);
    expect(sameBindings([{ ...claude, model: "n" }, codex], [claude, codex])).toBe(false);
  });
});

describe("filterRouterModels", () => {
  const models = [
    { id: "gpt-5.5", sources: ["Codex"], available: true },
    { id: "deepseek-flash", sources: ["DeepSeek", "AIROUTER"], available: true },
    { id: "claude-sonnet-4-5", sources: ["AIROUTER"], available: false }
  ];
  it("returns everything for an empty query", () => {
    expect(filterRouterModels(models, "  ")).toBe(models);
  });
  it("matches ids and source names, every word, case-insensitively", () => {
    expect(filterRouterModels(models, "airouter").map((m) => m.id)).toEqual([
      "deepseek-flash",
      "claude-sonnet-4-5"
    ]);
    expect(filterRouterModels(models, "Deep airouter").map((m) => m.id)).toEqual([
      "deepseek-flash"
    ]);
    expect(filterRouterModels(models, "gpt codex").map((m) => m.id)).toEqual(["gpt-5.5"]);
    expect(filterRouterModels(models, "nothing")).toEqual([]);
  });
});

describe("upstreamState", () => {
  const now = 1_000_000;
  it("follows the precedence disabled → unavailable → cooldown → error → ok → idle", () => {
    expect(upstreamState(cand("a", { enabled: false }), undefined, now).state).toBe("disabled");
    expect(
      upstreamState(cand("a", { available: false }), health({ requests: 5 }), now).state
    ).toBe("unavailable");
    expect(
      upstreamState(cand("a"), health({ cooldownUntil: now + 5_000, lastError: "HTTP 429" }), now)
    ).toEqual({ state: "cooldown", until: now + 5_000 });
    expect(
      upstreamState(cand("a"), health({ cooldownUntil: now - 1, lastError: "HTTP 500" }), now).state
    ).toBe("error");
    expect(upstreamState(cand("a"), health({ requests: 3 }), now).state).toBe("ok");
    expect(upstreamState(cand("a"), undefined, now).state).toBe("idle");
  });
});

describe("formatCountdown", () => {
  it("renders seconds then minutes and never goes negative", () => {
    expect(formatCountdown(1_045_000, 1_000_000)).toBe("45s");
    expect(formatCountdown(1_250_000, 1_000_000)).toBe("4m 10s");
    expect(formatCountdown(1_120_000, 1_000_000)).toBe("2m");
    expect(formatCountdown(900_000, 1_000_000)).toBe("0s");
  });
});

describe("candidate list edits", () => {
  const list = [cand("a"), cand("b", { enabled: false }), cand("c")];
  it("derives prefs in display order", () => {
    expect(prefsFromCandidates(list)).toEqual([
      { key: "a", enabled: true },
      { key: "b", enabled: false },
      { key: "c", enabled: true }
    ]);
  });
  it("toggles one key only", () => {
    const next = toggleCandidate(list, "b", true);
    expect(next.map((c) => c.enabled)).toEqual([true, true, true]);
    expect(list[1].enabled).toBe(false);
  });
  it("moves within bounds and is a no-op at the edges", () => {
    expect(moveCandidate(list, "c", "up").map((c) => c.key)).toEqual(["a", "c", "b"]);
    expect(moveCandidate(list, "a", "down").map((c) => c.key)).toEqual(["b", "a", "c"]);
    expect(moveCandidate(list, "a", "up")).toBe(list);
    expect(moveCandidate(list, "c", "down")).toBe(list);
    expect(moveCandidate(list, "zz", "up")).toBe(list);
  });
  it("never moves a row out of its tool group", () => {
    const grouped = [cand("a"), cand("b"), cand("x", { app: "codex" })];
    expect(canMoveCandidate(grouped, "b", "down")).toBe(false);
    expect(canMoveCandidate(grouped, "x", "up")).toBe(false);
    expect(canMoveCandidate(grouped, "b", "up")).toBe(true);
    expect(moveCandidate(grouped, "b", "down")).toBe(grouped);
  });
});

describe("parsePort", () => {
  it("accepts 1–65535 integers only", () => {
    expect(parsePort("8317")).toBe(8317);
    expect(parsePort(" 1 ")).toBe(1);
    expect(parsePort("0")).toBeNull();
    expect(parsePort("65536")).toBeNull();
    expect(parsePort("80a")).toBeNull();
    expect(parsePort("")).toBeNull();
  });
});

describe("candidateReason", () => {
  const t = (key: string) => `T:${key}`;
  it("translates a known reason code", () => {
    expect(
      candidateReason({ reason: "token expired — run codex once", reasonCode: "token_expired" }, t)
    ).toBe("T:router.reason.tokenExpired");
    expect(candidateReason({ reason: "x", reasonCode: "not_logged_in" }, t)).toBe(
      "T:router.reason.notLoggedIn"
    );
  });
  it("falls back to the raw reason for a null or unknown code", () => {
    expect(candidateReason({ reason: "free form", reasonCode: null }, t)).toBe("free form");
    expect(candidateReason({ reason: "new thing", reasonCode: "brand_new" }, t)).toBe("new thing");
    expect(candidateReason({ reason: "proto", reasonCode: "constructor" }, t)).toBe("proto");
    expect(candidateReason({ reason: null, reasonCode: null }, t)).toBeNull();
  });
});

describe("sameStoredList", () => {
  it("ignores key order and empty leaves", () => {
    const a = [{ id: "g", name: "", baseUrl: "u", bindings: [{ id: "b", app: "codex", model: undefined }] }];
    const b = [{ bindings: [{ app: "codex", id: "b" }], baseUrl: "u", id: "g" }];
    expect(sameStoredList(a, b)).toBe(true);
  });
  it("detects a changed value", () => {
    expect(sameStoredList([{ id: "g", apiKey: "k1" }], [{ id: "g", apiKey: "k2" }])).toBe(false);
    expect(sameStoredList([{ id: "g" }], [{ id: "g" }, { id: "h" }])).toBe(false);
  });
});

describe("listen address helpers", () => {
  it("maps stored hosts to the three choices", () => {
    expect(hostMode("127.0.0.1")).toBe("local");
    expect(hostMode("")).toBe("local");
    expect(hostMode("0.0.0.0")).toBe("lan");
    expect(hostMode("::")).toBe("lan");
    expect(hostMode("192.168.1.20")).toBe("interface");
  });
});
