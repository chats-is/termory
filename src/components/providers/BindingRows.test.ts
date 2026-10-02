import { describe, expect, it } from "vitest";
import { draftChecks, draftsFromGateway, withBindToggled } from "./BindingRows";
import { CLI_APPS } from "@/constants";
import type { CliApp, Gateway, GatewayProtocol } from "@/types";

const protocols = Object.fromEntries(
  CLI_APPS.map((a) => [a, ["anthropic", "openai"] as GatewayProtocol[]])
) as Record<CliApp, GatewayProtocol[]>;

function gateway(): Gateway {
  return {
    kind: "router",
    id: "gw",
    name: "Router",
    baseUrl: "http://127.0.0.1:8317",
    apiKey: "k",
    bindings: [
      { id: "b-claude", app: "claude", model: "saved-model" },
      { id: "b-gemini", app: "gemini" }
    ]
  };
}

describe("withBindToggled", () => {
  it("binds only the toggled app; other rows' unsaved edits are not committed", () => {
    const g = gateway();
    const drafts = draftsFromGateway(g);
    drafts.claude = { ...drafts.claude, model: "UNSAVED" };
    const out = withBindToggled(g.bindings, drafts, "codex", true, protocols, CLI_APPS);
    expect(out.map((b) => b.app)).toEqual(["claude", "codex", "gemini"]);
    expect(out.find((b) => b.app === "claude")?.model).toBe("saved-model");
    expect(out.find((b) => b.app === "codex")?.id).toBe(drafts.codex.id);
  });

  it("unbinds only the toggled app and keeps hidden bindings last", () => {
    const g = gateway();
    const drafts = draftsFromGateway(g);
    const visible = CLI_APPS.filter((a) => a !== "gemini");
    const out = withBindToggled(g.bindings, drafts, "claude", false, protocols, visible);
    expect(out.map((b) => b.id)).toEqual(["b-gemini"]);
  });
});

describe("draftChecks.appBlocked", () => {
  it("flags a multi-model app bound with no models, and only that app", () => {
    const drafts = draftsFromGateway(gateway());
    drafts.grok = { ...drafts.grok, checked: true };
    const checks = draftChecks(drafts, protocols);
    expect(checks.appBlocked("grok")).toBe(true);
    expect(checks.appBlocked("claude")).toBe(false);
  });
});
