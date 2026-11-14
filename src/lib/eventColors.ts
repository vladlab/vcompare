// Event marker colours, shared by the chart, the timeline bar and the event
// lists. Flashes are coloured by subkind (what the flash looked like),
// everything else by kind.

import type { DetectedEvent } from "./rpc";

export const EVENT_COLORS: Record<string, string> = {
  // flash subkinds
  black: "#9a9a9a",
  white: "#ffffff",
  content: "#e8b830",
  // timeline mode
  mismatch: "#e5534b",
  error: "#d29922",
  // repeats
  repeat: "#4cc38a",
  hold: "#2e8a5e",
  // image match
  match: "#f070c0",
};

export function eventColor(e: Pick<DetectedEvent, "kind" | "subkind">): string {
  if (e.kind === "flash") return EVENT_COLORS[e.subkind] ?? EVENT_COLORS.content;
  if (e.kind === "repeat") return e.subkind === "hold" ? EVENT_COLORS.hold : e.subkind === "black" ? EVENT_COLORS.black : EVENT_COLORS.repeat;
  return EVENT_COLORS[e.kind] ?? EVENT_COLORS.content;
}

/** Faded markers: flashes sitting on a cut. */
export function eventFaded(e: Pick<DetectedEvent, "kind" | "at_cut">): boolean {
  return e.kind === "flash" && e.at_cut;
}
