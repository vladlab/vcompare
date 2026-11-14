// Tabs under the chart, and what each detector's rows show.
//
// Adding a detector: an engine detector (src-tauri/src/engine/detect/), a
// `DetectorKind`, an entry in TABS, its defaults in DEFAULT_PARAMS, its
// controls in DetectorToolbar.svelte and its strip in `stripFor`.

import type {
  AnalysisMode,
  AnalysisSummary,
  DetectedEvent,
  DetectorKind,
  DetectRequest,
  FlashParams,
  MatchParams,
  RepeatParams,
} from "./rpc";

export type TabId = "frames" | "flash" | "repeat" | "match" | "report";

export interface TabDef {
  id: TabId;
  /** Label, possibly per mode */
  label: (mode: AnalysisMode) => string;
  /** Modes where the tab exists */
  modes: AnalysisMode[];
  /** Tabs that list events */
  detector?: DetectorKind;
  /** null = usable, else why not (shown greyed with this as tooltip/body) */
  unavailable?: (s: AnalysisSummary) => string | null;
}

export const TABS: TabDef[] = [
  { id: "frames", label: () => "Frames", modes: ["diff", "vmaf", "levels", "timeline"] },
  {
    id: "flash",
    label: (m) => (m === "timeline" ? "Mismatches" : "Flash frames"),
    modes: ["diff", "levels", "timeline"],
    detector: "flash",
    unavailable: (s) =>
      s.mode === "timeline" && s.timeline_run?.dry_run ? "A dry run only seeks and grabs: nothing was compared" : null,
  },
  {
    id: "repeat",
    label: () => "Repeats",
    modes: ["levels"],
    detector: "repeat",
  },
  {
    id: "match",
    label: () => "Image match",
    modes: ["levels"],
    detector: "match",
    unavailable: (s) =>
      s.has_signatures
        ? null
        : "This analysis has no frame signatures — re-run it with “keep frame signatures” on",
  },
  { id: "report", label: () => "Report", modes: ["timeline"] },
];

/** A tab as the strip shows it. */
export interface TabItem {
  id: TabId;
  label: string;
  count?: number | null;
  unavailable?: string | null;
}

export function tabsFor(mode: AnalysisMode | undefined): TabDef[] {
  return mode ? TABS.filter((t) => t.modes.includes(mode)) : [];
}

export interface DetectorParams {
  flash: FlashParams;
  repeat: RepeatParams;
  match: MatchParams;
}

export const DEFAULT_PARAMS: DetectorParams = {
  flash: { threshold: null, max_run: 3, exclude_at_cut: false },
  repeat: { tolerance: 0, min_run: 1, max_run: 5, include_long: false, skip_black: true },
  match: { references: [], min_similarity: 0.9, min_run: 1 },
};

export function requestFor<K extends DetectorKind>(kind: K, params: DetectorParams[K]): DetectRequest {
  return { detector: kind, ...params } as DetectRequest;
}

/** What a row's filmstrip shows. `roles[i]` labels tile i. */
export interface Strip {
  start: number;
  count: number;
  roles: ("prev" | "run" | "next")[];
  /** Event frames not shown (long runs) */
  more: number;
}

/** Longest run of event frames a strip shows before "+n". */
const RUN_TILES: Record<DetectorKind, number> = { flash: 5, repeat: 3, match: 2 };

/** The strip for an event: the frame before, the event's frames (capped),
 *  and the frame after when the run is shown whole. */
export function stripFor(kind: DetectorKind, e: DetectedEvent): Strip {
  const shown = Math.min(e.length, RUN_TILES[kind]);
  const whole = shown === e.length;
  const roles: Strip["roles"] = ["prev", ...Array<"run">(shown).fill("run")];
  if (whole) roles.push("next");
  return { start: e.frame - 1, count: roles.length, roles, more: e.length - shown };
}

/** Tiles a strip can have at most for these parameters (keeps columns aligned). */
export function maxTiles(kind: DetectorKind, params: DetectorParams): number {
  if (kind === "flash") return Math.min(params.flash.max_run ?? 3, RUN_TILES.flash) + 2;
  return RUN_TILES[kind] + 2;
}
