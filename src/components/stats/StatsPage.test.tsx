import React from "react";
import { describe, expect, it, vi } from "vitest";
import {
  render as rtlRender,
  screen,
  fireEvent,
  within,
  type RenderOptions
} from "@testing-library/react";
import { TooltipProvider } from "@/components/ui/tooltip";
import { StatsPage } from "./StatsPage";
import type { AppSession } from "@/types";

// jsdom lacks ResizeObserver (recharts ResponsiveContainer wants it).
// Kept local to the test file per the project convention.
class RO {
  observe() {}
  unobserve() {}
  disconnect() {}
}
(globalThis as { ResizeObserver?: unknown }).ResizeObserver ??= RO;

function render(ui: React.ReactElement, options?: RenderOptions) {
  return rtlRender(<TooltipProvider>{ui}</TooltipProvider>, options);
}

function mk(partial: Partial<AppSession>): AppSession {
  return {
    id: "x",
    source: "Claude",
    title: "t",
    project: "",
    path: "/p",
    started_at: "2026-07-01T10:00:00",
    updated_at: "2026-07-01T11:00:00",
    message_count: 0,
    preview: "",
    snippet: "",
    message_previews: [],
    model: "claude-opus-4-8",
    daily_tokens: [
      {
        date: "2026-07-01",
        tokens: { input: 100, output: 50, cached: 0, reasoning: 0, total: 150 },
        messages: 3
      }
    ],
    ...partial
  };
}

describe("StatsPage", () => {
  it("renders the Overview KPIs and the Tokens chart (type mode) on one page", () => {
    render(
      <StatsPage sessions={[mk({})]} onRefresh={() => {}} refreshing={false} />
    );
    // Overview content: the KPI labels.
    expect(screen.getByText("Active days")).toBeTruthy();
    expect(screen.getByText("Favorite model")).toBeTruthy();
    // Tokens chart defaults to type mode: its legend renders
    // alongside the KPIs with no interaction needed.
    expect(screen.getByText("Cached")).toBeTruthy();
  });

  it("switches the Tokens chart to model mode via the toggle", () => {
    render(
      <StatsPage sessions={[mk({})]} onRefresh={() => {}} refreshing={false} />
    );
    // Type legend visible by default, model legend not yet.
    expect(screen.queryByText("100 in · 50 out")).toBeNull();
    fireEvent.mouseDown(screen.getByRole("tab", { name: "Model" }), {
      button: 0
    });
    // Model mode's legend now shows the per-model in/out line.
    expect(screen.getByText("100 in · 50 out")).toBeTruthy();
    expect(screen.queryByText("Cached")).toBeNull();
  });

  it("colors a model by its provider FAMILY, stepped by its ALL-TIME rank within the provider (not its window rank)", () => {
    // Both models are Claude, so both take the CLAY family ramp — a Claude
    // model never gets a "custom" pool color. claude-opus-4-8's only
    // activity is recent (inside the default 30d window), so it's the ONLY
    // Claude model with in-window tokens — window-local rank 0.
    // claude-sonnet-4-6's activity is far in the past (outside the 30d
    // window) but massive, so it dominates ALL-TIME Claude usage — global
    // rank 0 in the Claude group. claude-opus-4-8 must therefore get the
    // rank-1 clay shade (--stat-claude-1), not the rank-0 anchor
    // (--stat-claude-0) it would get if colors tracked the window rank.
    const opus = mk({
      id: "opus",
      model: "claude-opus-4-8",
      daily_tokens: [
        {
          date: "2026-07-01",
          tokens: { input: 500, output: 500, cached: 0, reasoning: 0, total: 1000 },
          messages: 5
        }
      ]
    });
    const oldSonnet = mk({
      id: "sonnet-old",
      model: "claude-sonnet-4-6",
      started_at: "2025-01-01T10:00:00",
      updated_at: "2025-01-01T11:00:00",
      daily_tokens: [
        {
          date: "2025-01-01",
          tokens: {
            input: 500000,
            output: 500000,
            cached: 0,
            reasoning: 0,
            total: 1000000
          },
          messages: 500
        }
      ]
    });
    render(
      <StatsPage
        sessions={[opus, oldSonnet]}
        onRefresh={() => {}}
        refreshing={false}
      />
    );
    fireEvent.mouseDown(screen.getByRole("tab", { name: "Model" }), {
      button: 0
    });
    // The raw id "claude-opus-4-8" also appears as the "Favorite model" KPI
    // value — scope to the legend row (a <span>, not the KPI card's <div>).
    const legendLabel = screen
      .getAllByText("claude-opus-4-8")
      .find((el) => el.tagName === "SPAN");
    const dot = legendLabel?.closest("div")?.querySelector("span[aria-hidden]");
    // The rank-1 clay shade (claude-opus-4-8 is the Claude group's #2
    // all-time model) — NOT the rank-0 anchor, which the window-local rank
    // would assign since claude-opus-4-8 is alone in the current 30d window.
    expect(dot).toHaveStyle({ background: "var(--stat-claude-1)" });
  });

  it("recognizes newly-added mainstream vendors (DeepSeek / Mistral / Qwen / Grok / GLM / MiniMax) as their own family, not the custom pool", () => {
    // One model per newly-added vendor (incl. a gateway `vendor/` prefix and
    // the Mistral `codestral` / Qwen `qwq` / GLM `chatglm` / MiniMax `abab`
    // family aliases), each the sole model of its provider → rank 0 → the
    // provider's anchor shade.
    const cases: { model: string; anchor: string }[] = [
      { model: "deepseek-v3", anchor: "var(--stat-deepseek-0)" },
      { model: "mistralai/codestral-latest", anchor: "var(--stat-mistral-0)" },
      { model: "qwq-32b", anchor: "var(--stat-qwen-0)" },
      { model: "grok-4", anchor: "var(--stat-grok-0)" },
      { model: "chatglm-4", anchor: "var(--stat-glm-0)" },
      { model: "abab6.5s", anchor: "var(--stat-minimax-0)" }
    ];
    const sessions = cases.map((c, i) =>
      mk({
        id: c.model,
        source: "Codex",
        model: c.model,
        daily_tokens: [
          {
            date: "2026-07-01",
            tokens: {
              input: 10,
              output: 10,
              cached: 0,
              reasoning: 0,
              total: (cases.length - i) * 100
            },
            messages: 1
          }
        ]
      })
    );
    render(
      <StatsPage sessions={sessions} onRefresh={() => {}} refreshing={false} />
    );
    fireEvent.mouseDown(screen.getByRole("tab", { name: "Model" }), {
      button: 0
    });
    for (const c of cases) {
      const label = screen
        .getAllByText(c.model)
        .find((el) => el.tagName === "SPAN");
      const dot = label?.closest("div")?.querySelector("span[aria-hidden]");
      expect(dot).toHaveStyle({ background: c.anchor });
    }
  });

  it("model-mode legend mirrors the chart 1:1 — one Others row, percentages sum to 100%", () => {
    const named = ["m1", "m2", "m3", "m4", "m5", "m6", "m7"].map((m, i) =>
      mk({
        id: m,
        model: m,
        daily_tokens: [
          {
            date: "2026-07-01",
            tokens: {
              input: 10,
              output: 10,
              cached: 0,
              reasoning: 0,
              total: (7 - i) * 100
            },
            messages: 1
          }
        ]
      })
    );
    const unknown = mk({
      id: "u",
      model: "",
      daily_tokens: [
        {
          date: "2026-07-01",
          tokens: { input: 5, output: 5, cached: 0, reasoning: 0, total: 50 },
          messages: 1
        }
      ]
    });
    render(
      <StatsPage
        sessions={[...named, unknown]}
        onRefresh={() => {}}
        refreshing={false}
      />
    );
    fireEvent.mouseDown(screen.getByRole("tab", { name: "Model" }), {
      button: 0
    });
    // Top 6 (by tokens desc: m1..m6) each get their own legend row. "m1"
    // also appears as the "Favorite model" KPI value, so require at
    // least one match rather than a single exact one.
    for (const m of ["m1", "m2", "m3", "m4", "m5", "m6"]) {
      expect(screen.getAllByText(m).length).toBeGreaterThan(0);
    }
    // m7 folds into Others (no dedicated row) — exactly ONE Others row,
    // not one per folded model.
    expect(screen.queryByText("m7")).toBeNull();
    expect(screen.getAllByText("Others")).toHaveLength(1);
    // Percentages (top 6 + Others) sum to 100%, since Others absorbs m7
    // AND the Unknown session's tokens.
    const percents = screen
      .getAllByText(/^\d+\.\d%$/)
      .map((el) => parseFloat(el.textContent!));
    expect(percents).toHaveLength(7);
    expect(Math.round(percents.reduce((a, b) => a + b, 0))).toBe(100);
  });

  it("offers the All / 30d / 7d range pills", () => {
    render(<StatsPage sessions={[]} onRefresh={() => {}} refreshing={false} />);
    // Scope to the Range tablist — the source filter also has an "All".
    const rangeList = within(screen.getByRole("tablist", { name: "Range" }));
    for (const label of ["All", "30d", "7d"]) {
      expect(rangeList.getByRole("tab", { name: label })).toBeTruthy();
    }
    const seven = rangeList.getByRole("tab", { name: "7d" });
    fireEvent.mouseDown(seven, { button: 0 });
    expect(seven.getAttribute("data-state")).toBe("active");
  });

  it("filters by source via the brand pills", () => {
    render(
      <StatsPage
        sessions={[mk({}), mk({ source: "Codex", model: "gpt-5", id: "y" })]}
        onRefresh={() => {}}
        refreshing={false}
      />
    );
    const codex = screen.getByRole("tab", { name: /Codex/ });
    fireEvent.mouseDown(codex, { button: 0 });
    expect(codex.getAttribute("data-state")).toBe("active");
  });

  it("hides a disabled tool's pill (Settings → Tools)", () => {
    render(
      <StatsPage
        sessions={[mk({}), mk({ source: "Codex", model: "gpt-5", id: "y" })]}
        onRefresh={() => {}}
        refreshing={false}
        sourceToggles={{ codex: false }}
      />
    );
    expect(screen.queryByRole("tab", { name: /Codex/ })).toBeNull();
    expect(screen.getByRole("tab", { name: /Claude/ })).toBeInTheDocument();
  });

  it("wires the refresh button", () => {
    const onRefresh = vi.fn();
    render(
      <StatsPage sessions={[]} onRefresh={onRefresh} refreshing={false} />
    );
    fireEvent.click(screen.getByRole("button", { name: "Refresh stats" }));
    expect(onRefresh).toHaveBeenCalledTimes(1);
  });
});
