// Thumbnail strips for event rows, loaded lazily.
//
// Every event row shows a short filmstrip (frame before, the event's
// frames, frame after). Each strip is one ffmpeg seek in the engine, so
// loading is held back and bounded:
//
// - Settle: after a detector parameter changes (threshold drag, toggles)
//   nothing loads until `SETTLE_MS` of quiet. Opening an analysis or
//   switching tabs uses a short delay instead.
// - Visible first: the queue is what the list says is on screen (the
//   picked event first, then visible rows, then a screenful below). It is
//   replaced, not appended to, whenever the list scrolls or changes.
// - Bounded: at most `MAX_IN_FLIGHT` requests (the engine caps it too).
// - Cached by span (first frame + count), independent of the threshold, so
//   events that survive a parameter change keep their tiles. LRU.
//
// A request already running when the queue is replaced is left to finish:
// its strip is still right for that span, so it is cached.

import { SvelteMap } from "svelte/reactivity";
import { previewFrames, type PreviewRow } from "./rpc";

export const SETTLE_MS = 2500;
export const QUICK_MS = 300;
const SCROLL_MS = 150;
const MAX_IN_FLIGHT = 3;
const CACHE_LIMIT = 1500;
/** Requested at twice the display height (HiDPI). */
export const THUMB_REQUEST_HEIGHT = 108;

export interface Span {
  /** First display frame of the strip */
  start: number;
  count: number;
}

export function spanKey(s: Span): string {
  return `${s.start}:${s.count}`;
}

export class ThumbLoader {
  /** key → rows (one per file), or "error" */
  readonly strips = new SvelteMap<string, PreviewRow[] | "error">();
  private queue: Span[] = [];
  private inFlight = new Set<string>();
  private timer: ReturnType<typeof setTimeout> | null = null;
  private heldUntil = 0;
  private reportedError = false;
  /** Called once per hold/settle cycle with the first failure. */
  onError: ((msg: string) => void) | null = null;

  get(s: Span): PreviewRow[] | "error" | undefined {
    return this.strips.get(spanKey(s));
  }

  /** The list's wishes, in priority order. Replaces the previous queue. */
  want(spans: Span[]) {
    const seen = new Set<string>();
    this.queue = spans.filter((s) => {
      const k = spanKey(s);
      if (seen.has(k) || this.strips.has(k) || this.inFlight.has(k)) return false;
      seen.add(k);
      return true;
    });
    if (this.queue.length > 0) this.kick(SCROLL_MS);
  }

  /** Parameters changed: load nothing until `ms` of quiet. */
  hold(ms: number = SETTLE_MS) {
    this.heldUntil = Date.now() + ms;
    this.reportedError = false;
    this.schedule(ms);
  }

  /** Forget everything (another analysis was opened). */
  clear() {
    this.queue = [];
    this.strips.clear();
    this.reportedError = false;
    if (this.timer) clearTimeout(this.timer);
    this.timer = null;
  }

  private kick(ms: number) {
    const wait = Math.max(ms, this.heldUntil - Date.now());
    this.schedule(wait);
  }

  private schedule(ms: number) {
    if (this.timer) clearTimeout(this.timer);
    this.timer = setTimeout(() => {
      this.timer = null;
      this.pump();
    }, Math.max(0, ms));
  }

  private pump() {
    if (Date.now() < this.heldUntil) {
      this.schedule(this.heldUntil - Date.now());
      return;
    }
    while (this.inFlight.size < MAX_IN_FLIGHT && this.queue.length > 0) {
      const s = this.queue.shift()!;
      const k = spanKey(s);
      if (this.strips.has(k) || this.inFlight.has(k)) continue;
      this.inFlight.add(k);
      previewFrames(s.start, s.count, THUMB_REQUEST_HEIGHT)
        .then((r) => this.put(k, r.rows))
        .catch((e) => {
          this.put(k, "error");
          if (!this.reportedError) {
            this.reportedError = true;
            this.onError?.(String(e));
          }
        })
        .finally(() => {
          this.inFlight.delete(k);
          this.pump();
        });
    }
  }

  private put(k: string, v: PreviewRow[] | "error") {
    this.strips.delete(k); // re-insert = most recently used
    this.strips.set(k, v);
    while (this.strips.size > CACHE_LIMIT) {
      const oldest = this.strips.keys().next().value;
      if (oldest === undefined) break;
      this.strips.delete(oldest);
    }
  }

  /** A failed strip is retried when its row comes back into view. */
  forgetErrors(spans: Span[]) {
    for (const s of spans) {
      const k = spanKey(s);
      if (this.strips.get(k) === "error") this.strips.delete(k);
    }
  }
}
