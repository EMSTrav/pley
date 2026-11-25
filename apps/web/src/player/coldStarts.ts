import type HlsType from "hls.js";
import type { Fragment, HlsConfig, Level, LoaderCallbacks, LoaderConfiguration, LoaderContext, LoaderStats } from "hls.js";

/**
 * Whether `frag` starts a size: the first segment after a switch, a seek or
 * the start, not the one after `last`. The transcoder makes only the size
 * the player asks for, so it starts that size then, which takes seconds
 * (docs/QUALITY.md).
 */
export function startsASize(last: { level: number; sn: number } | null, frag: { level: number; sn: number }): boolean {
  return !last || frag.level !== last.level || frag.sn !== last.sn + 1;
}

/** No step up for `HOLD_MS` after a step down… */
export const HOLD_MS = 30_000;
/** …doubled, up to this, each time a step up is followed by a step down within it. */
export const MAX_HOLD_MS = 240_000;

/**
 * When hls.js may move up a size. A step down holds it there a while, so a
 * size that came slowly isn't tried again at once: hls.js counts only the
 * network, and a size the transcoder can't keep up with would otherwise
 * come and go every few segments. A step up that soon steps down again
 * holds it longer.
 */
export class HoldDown {
  private downAt = Number.NEGATIVE_INFINITY;
  private upAt = Number.NEGATIVE_INFINITY;
  private hold = HOLD_MS;
  /** The size hls.js loads (its levels go up with the bitrate), or -1 before the first. */
  level = -1;

  /** hls.js moves to `to`. */
  switched(to: number, now: number) {
    if (this.level >= 0 && to < this.level) {
      if (now - this.upAt < this.hold) this.hold = Math.min(this.hold * 2, MAX_HOLD_MS);
      this.downAt = now;
    } else if (this.level >= 0 && to > this.level) {
      this.upAt = now;
    }
    this.level = to;
  }

  /** The size to load next, of the `next` hls.js picks. */
  allow(next: number, now: number): number {
    return this.level >= 0 && next > this.level && now - this.downAt < this.hold ? this.level : next;
  }
}

type LoaderClass = new (config: HlsConfig) => {
  load(context: LoaderContext, config: LoaderConfiguration, callbacks: LoaderCallbacks<LoaderContext>): void;
};
/** What of hls.js's ABR controller this reaches: members its types keep private. */
interface AbrInternals {
  _abandonRulesCheck: (levelLoaded?: Level) => void;
  fragCurrent: Fragment | null;
}

/**
 * hls.js settings for a transcode in several sizes, where the transcoder
 * starts a size when the player asks for it. hls.js takes the wait for a
 * segment's first byte as a slow network: after a seek, or as it moves to
 * another size, it would drop to the smallest size at once, which starts
 * from cold too, then go back up, again and again. For a segment that
 * starts a size, that wait isn't counted, and no emergency switch down is
 * made while it lasts; and a step down holds a while ([`HoldDown`]).
 * Segments after it count in full, so a size the transcoder can't make in
 * real time still steps down. `{}` where hls.js's own classes aren't at hand.
 */
export function coldStartConfig(Hls: typeof HlsType): Partial<HlsConfig> {
  const defaults = Hls.DefaultConfig as Partial<HlsConfig> | undefined;
  const BaseLoader = defaults?.loader as LoaderClass | undefined;
  const BaseAbr = defaults?.abrController as (new (hls: HlsType) => object) | undefined;
  if (!BaseLoader || !BaseAbr) return {};
  // The getter and setter Automatic's choice goes through.
  const autoLevel = Object.getOwnPropertyDescriptor(BaseAbr.prototype, "nextAutoLevel");
  const starting = new WeakSet<object>();
  let last: { level: number; sn: number } | null = null;

  class Loader extends BaseLoader {
    load(context: LoaderContext, config: LoaderConfiguration, callbacks: LoaderCallbacks<LoaderContext>) {
      const frag = (context as LoaderContext & { frag?: Fragment }).frag;
      if (frag && typeof frag.sn === "number") {
        const at = { level: frag.level, sn: frag.sn };
        if (startsASize(last, at)) {
          starting.add(frag);
          const done = callbacks.onSuccess;
          callbacks = {
            ...callbacks,
            onSuccess: (response, stats: LoaderStats, ctx, details) => {
              // Counted from the first byte: the wait before was the transcoder's.
              if (stats.loading.first > stats.loading.start) stats.loading.start = stats.loading.first;
              done(response, stats, ctx, details);
            },
          };
        }
        last = at;
      }
      super.load(context, config, callbacks);
    }
  }

  class Abr extends BaseAbr {
    private readonly held = new HoldDown();
    constructor(hls: HlsType) {
      super(hls);
      const self = this as unknown as AbrInternals;
      const check = self._abandonRulesCheck;
      self._abandonRulesCheck = (levelLoaded?: Level) => {
        const frag = self.fragCurrent;
        if (frag && starting.has(frag) && !frag.stats.loading.first) return;
        check(levelLoaded);
      };
      hls.on(Hls.Events.LEVEL_SWITCHING, (_event, data) => this.held.switched(data.level, performance.now()));
    }
    get nextAutoLevel(): number {
      const next = (autoLevel?.get?.call(this) as number | undefined) ?? -1;
      return this.held.allow(next, performance.now());
    }
    set nextAutoLevel(level: number) {
      autoLevel?.set?.call(this, level);
    }
  }

  return { fLoader: Loader as unknown as HlsConfig["fLoader"], abrController: Abr as unknown as HlsConfig["abrController"] };
}
