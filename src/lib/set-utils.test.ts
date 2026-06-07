import { describe, expect, it } from "vitest";
import { addSetValue, toggleSetValue } from "./set-utils";

describe("addSetValue", () => {
  it("returns a NEW set with the value added, leaving the original untouched", () => {
    const original = new Set([1, 2]);
    const next = addSetValue(original, 3);
    expect(next).toEqual(new Set([1, 2, 3]));
    expect(original).toEqual(new Set([1, 2]));
    expect(next).not.toBe(original);
  });

  it("still returns a fresh set when the value is already present", () => {
    const original = new Set([1]);
    const next = addSetValue(original, 1);
    expect(next).toEqual(new Set([1]));
    expect(next).not.toBe(original);
  });
});

describe("toggleSetValue", () => {
  it("adds the value when absent", () => {
    expect(toggleSetValue(new Set([1]), 2)).toEqual(new Set([1, 2]));
  });

  it("removes the value when present", () => {
    expect(toggleSetValue(new Set([1, 2]), 2)).toEqual(new Set([1]));
  });

  it("does not mutate the original set", () => {
    const original = new Set([1, 2]);
    const next = toggleSetValue(original, 2);
    expect(original).toEqual(new Set([1, 2]));
    expect(next).not.toBe(original);
  });
});
