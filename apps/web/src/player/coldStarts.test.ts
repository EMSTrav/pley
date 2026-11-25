import { describe, expect, it } from "vitest";
import type HlsType from "hls.js";
import { coldStartConfig, HOLD_MS, HoldDown, MAX_HOLD_MS, startsASize } from "./coldStarts";

describe("a segment that starts a size", () => {
  it("is the first, the first after a switch, and the first after a seek", () => {
    expect(startsASize(null, { level: 3, sn: 0 })).toBe(true);
    expect(startsASize({ level: 3, sn: 4 }, { level: 3, sn: 5 })).toBe(false);
    expect(startsASize({ level: 3, sn: 5 }, { level: 1, sn: 6 })).toBe(true);
    expect(startsASize({ level: 3, sn: 5 }, { level: 3, sn: 50 })).toBe(true);
    expect(startsASize({ level: 3, sn: 5 }, { level: 3, sn: 5 })).toBe(true);
  });
});

describe("a step down holding", () => {
  it("lets hls.js move freely until it steps down", () => {
    const held = new HoldDown();
    expect(held.allow(3, 0)).toBe(3);
    held.switched(1, 0);
    held.switched(3, 1000);
    expect(held.allow(2, 2000)).toBe(2);
  });

  it("holds the size stepped down to, then lets it up", () => {
    const held = new HoldDown();
    held.switched(3, 0);
    held.switched(0, 10_000);
    expect(held.allow(3, 10_000 + HOLD_MS - 1)).toBe(0);
    // Further down at once, whatever the hold.
    held.switched(1, 11_000);
    expect(held.allow(0, 12_000)).toBe(0);
    expect(held.allow(3, 11_000 + HOLD_MS)).toBe(3);
  });

  it("holds longer each time a step up soon steps down again", () => {
    const held = new HoldDown();
    held.switched(3, 0);
    held.switched(0, 0);
    let now = HOLD_MS;
    held.switched(3, now);
    held.switched(0, now + 5_000);
    now += 5_000;
    expect(held.allow(3, now + HOLD_MS)).toBe(0);
    expect(held.allow(3, now + 2 * HOLD_MS)).toBe(3);
    // Doubling stops at the most it holds.
    for (let i = 0; i < 6; i++) {
      now += MAX_HOLD_MS;
      held.switched(3, now);
      held.switched(0, now + 1_000);
      now += 1_000;
    }
    expect(held.allow(3, now + MAX_HOLD_MS - 1)).toBe(0);
    expect(held.allow(3, now + MAX_HOLD_MS)).toBe(3);
  });
});

describe("the settings for hls.js", () => {
  it("change nothing where hls.js's own classes aren't at hand", () => {
    expect(coldStartConfig({} as unknown as typeof HlsType)).toEqual({});
  });
});
