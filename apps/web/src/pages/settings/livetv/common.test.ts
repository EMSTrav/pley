import { describe, expect, it } from "vitest";
import type { LivePlan } from "@/api/livetv";
import { planLine } from "./common";

const plan = (over: Partial<LivePlan>): LivePlan => ({
  video_codec: "hevc",
  width: 3840,
  height: 2160,
  frame_rate: 60,
  interlaced: false,
  audio_codec: "ac3",
  audio_channels: 6,
  video: "h264_qsv",
  audio: "aac",
  ...over,
});

describe("planLine", () => {
  it("names the size a channel is scaled down to", () => {
    expect(planLine(plan({ out_height: 1080 }))).toBe("2160p HEVC → 1080p H.264 on Intel Quick Sync · Dolby Digital → AAC");
  });

  it("names no size when it keeps its own, or the transcoder doesn't say", () => {
    expect(planLine(plan({ out_height: 2160 }))).toBe("2160p HEVC → H.264 on Intel Quick Sync · Dolby Digital → AAC");
    expect(planLine(plan({}))).toBe("2160p HEVC → H.264 on Intel Quick Sync · Dolby Digital → AAC");
  });

  it("says a channel sent as it is so", () => {
    const copied = plan({ video_codec: "h264", height: 1080, video: "copy", audio_codec: "aac", audio: "copy", out_height: 1080 });
    expect(planLine(copied)).toBe("1080p H.264 as it is · AAC as it is");
  });
});
