<!--
  EventList — the events of one detector, one row each, with a filmstrip
  of the frames around the event in every row.

  Virtualised (fixed row height), so a few thousand events cost nothing.
  Strips come from the shared ThumbLoader: this component only tells it
  which spans are on screen (picked event first, then visible rows, then a
  screenful below); the loader decides when to fetch them.

  Props:
    kind        — which detector (decides the strip, see detectors.ts)
    events      — rows to show (already filtered)
    loader      — ThumbLoader
    tiles       — tile slots to reserve per file, so columns line up
    cursorFrame, selectedFrame, seen
    onSelect(frame), onReview(frame, verdict | null)
    timeline    — timeline mode labels
    refThumbs   — match: reference id → thumbnail, shown as the first tile
    empty       — text when there are no events
-->
<script lang="ts">
  import type { DetectedEvent, DetectorKind, EventReview, PreviewRow } from "../lib/rpc";
  import { stripFor, type Strip } from "../lib/detectors";
  import { eventColor, eventFaded } from "../lib/eventColors";
  import type { ThumbLoader } from "../lib/thumbLoader.svelte";
  import { untrack } from "svelte";

  interface Props {
    kind: DetectorKind;
    events: DetectedEvent[];
    loader: ThumbLoader;
    tiles: number;
    /** Files per strip (1, or 2 for a comparison) — for placeholders */
    files: number;
    cursorFrame: number;
    selectedFrame: number | null;
    seen: Set<number>;
    onSelect: (frame: number) => void;
    onReview: (frame: number, review: EventReview | null) => void;
    timeline?: boolean;
    /** The detector's threshold (0..1), to show flash strength in the
     *  slider's own units */
    threshold?: number | null;
    refThumbs?: Record<string, string | null | undefined>;
    empty: string;
  }

  let {
    kind,
    events,
    loader,
    tiles,
    files,
    cursorFrame,
    selectedFrame,
    seen,
    onSelect,
    onReview,
    timeline = false,
    threshold = null,
    refThumbs = {},
    empty,
  }: Props = $props();

  const ROW_HEIGHT = 66;
  const BUFFER = 4;

  let listEl: HTMLDivElement | undefined = $state();
  let scrollTop = $state(0);
  let viewportHeight = $state(400);

  let visibleStart = $derived(Math.max(0, Math.floor(scrollTop / ROW_HEIGHT) - BUFFER));
  let visibleEnd = $derived(Math.min(events.length, Math.ceil((scrollTop + viewportHeight) / ROW_HEIGHT) + BUFFER));
  let visible = $derived(events.slice(visibleStart, visibleEnd));

  let activeIndex = $derived(
    events.findIndex((e) => cursorFrame >= e.frame && cursorFrame < e.frame + e.length)
  );
  let selectedIndex = $derived(selectedFrame == null ? -1 : events.findIndex((e) => e.frame === selectedFrame));

  // ── Tell the loader what is on screen ──
  let lastVisibleKeys = new Set<string>();
  $effect(() => {
    const strips: Strip[] = [];
    const sel = selectedIndex >= 0 ? events[selectedIndex] : null;
    if (sel) strips.push(stripFor(kind, sel));
    const onScreenFirst = Math.floor(scrollTop / ROW_HEIGHT);
    const rowsPerScreen = Math.ceil(viewportHeight / ROW_HEIGHT);
    const onScreen = events.slice(onScreenFirst, onScreenFirst + rowsPerScreen + 1).map((e) => stripFor(kind, e));
    const below = events
      .slice(onScreenFirst + rowsPerScreen + 1, onScreenFirst + 2 * rowsPerScreen + 1)
      .map((e) => stripFor(kind, e));
    // Rows coming (back) into view get another try at a failed strip.
    const keys = new Set(onScreen.map((s) => `${s.start}:${s.count}`));
    // untrack: the loader's cache must not make this effect re-run every
    // time a strip lands.
    untrack(() => {
      loader.forgetErrors(onScreen.filter((s) => !lastVisibleKeys.has(`${s.start}:${s.count}`)));
      lastVisibleKeys = keys;
      loader.want([...strips, ...onScreen, ...below]);
    });
  });

  // Keep the picked row in view when navigating with the keyboard.
  $effect(() => {
    if (selectedIndex < 0 || !listEl) return;
    const top = selectedIndex * ROW_HEIGHT;
    if (top < listEl.scrollTop) listEl.scrollTop = top;
    else if (top + ROW_HEIGHT > listEl.scrollTop + listEl.clientHeight)
      listEl.scrollTop = top + ROW_HEIGHT - listEl.clientHeight;
  });

  function label(e: DetectedEvent): string {
    const len = e.length === 1 ? "1 fr" : `${e.length} fr`;
    switch (e.kind) {
      case "error":
        return `${e.label ?? ""} · not checked: ${e.subkind.replace(/_/g, " ")}`;
      case "mismatch":
        return e.label ?? e.subkind;
      case "repeat":
        return e.subkind === "hold" ? `held ${len}` : e.subkind === "black" ? `black, repeated ${len}` : `repeated ${len}`;
      case "match":
        return `${e.label ?? e.subkind} · ${len}`;
      default:
        return `${e.subkind} · ${len}${e.at_cut ? " · at cut" : ""}`;
    }
  }

  function score(e: DetectedEvent): string {
    if (e.kind === "error") return "—";
    if (e.kind === "repeat") return e.score >= 1 && Number.isInteger(e.score) ? "" : `×${e.score.toFixed(1)}`;
    if (e.kind === "match") return `×${e.score.toFixed(2)}`;
    // Flashes and mismatches: the change itself, in the threshold slider's
    // units (score is change ÷ threshold), so the number doesn't move when
    // the slider does and reads as "set the slider above this to drop it".
    if (threshold != null && threshold > 0) {
      const pct = e.score * threshold * 100;
      return `${pct.toFixed(pct < 10 ? (timeline ? 2 : 1) : 0)}%`;
    }
    return `×${e.score.toFixed(1)}`;
  }

  function scoreTitle(e: DetectedEvent): string {
    if (e.kind === "flash")
      return "How big the flash is: the smaller of the change going in and coming out, as % of peak — the same units as the threshold slider. Above the slider = listed.";
    if (e.kind === "mismatch") return "RMS luma difference from the SDI picture, % of peak (the threshold slider's units)";
    if (e.kind === "match") return "similarity relative to the minimum";
    return "";
  }

  function roleLabel(role: string, e: DetectedEvent, k: number): string {
    if (role === "prev") return e.kind === "repeat" ? "shown" : "prev";
    if (role === "next") return "next";
    if (timeline) return "sample";
    if (e.kind === "repeat") return "repeat";
    if (e.kind === "match") return "match";
    return e.length > 1 ? `flash ${k}` : "flash";
  }

  function review(e: DetectedEvent, r: EventReview) {
    onReview(e.frame, e.review === r ? null : r);
  }

  function rowsOf(strip: Strip): PreviewRow[] | "error" | undefined {
    return loader.get(strip);
  }
</script>

<div
  class="list"
  bind:this={listEl}
  bind:clientHeight={viewportHeight}
  onscroll={() => (scrollTop = listEl?.scrollTop ?? 0)}
>
  {#if events.length === 0}
    <p class="empty">{empty}</p>
  {:else}
    <div class="spacer" style="height: {events.length * ROW_HEIGHT}px">
      <div class="window" style="transform: translateY({visibleStart * ROW_HEIGHT}px)">
        {#each visible as e, j (e.frame)}
          {@const i = visibleStart + j}
          {@const strip = stripFor(kind, e)}
          {@const rows = rowsOf(strip)}
          <!-- svelte-ignore a11y_click_events_have_key_events -->
          <!-- svelte-ignore a11y_no_static_element_interactions -->
          <div
            class="row"
            style="height: {ROW_HEIGHT}px"
            class:active={i === activeIndex}
            class:selected={i === selectedIndex}
            class:seen={seen.has(e.frame)}
            class:accepted={e.review === "accept"}
            class:rejected={e.review === "reject"}
            onclick={() => onSelect(e.frame)}
          >
            <span class="dot" class:faded={eventFaded(e)} style="background: {eventColor(e)}"></span>
            <div class="ident">
              <span class="tc mono">
                {e.tc || e.frame}{#if seen.has(e.frame) && !e.review}<span class="seen-mark" title="visited"> ·</span>{/if}
              </span>
              <span class="desc" title={label(e)}>{label(e)}</span>
            </div>

            <div class="strips" class:loading={rows === undefined}>
              {#if kind === "match"}
                <div class="tile ref" title="reference: {e.label ?? e.subkind}">
                  {#if refThumbs[e.subkind]}<img src={refThumbs[e.subkind]} alt="reference" />{:else}<span class="blank">ref</span>{/if}
                  <span class="role">ref</span>
                </div>
              {/if}
              {#each Array.from({ length: Array.isArray(rows) ? rows.length : files }, (_, r) => r) as r (r)}
                {@const row = Array.isArray(rows) ? rows[r] : null}
                <div class="strip" style="min-width: {tiles * 100}px">
                  {#if files > 1}<span class="file-label">{row?.label ?? (r === 0 ? "REF" : "DIST")}</span>{/if}
                  {#each strip.roles as role, k (k)}
                    {@const f = row?.frames[k]}
                    <button
                      class="tile {role}"
                      class:at-cursor={f != null && f.frame === cursorFrame}
                      title={f ? `${f.tc} — click to go there` : ""}
                      disabled={!f}
                      onclick={(ev) => {
                        ev.stopPropagation();
                        if (f) onSelect(f.frame);
                        (ev.currentTarget as HTMLElement).blur();
                      }}
                    >
                      {#if f?.src}
                        <img src={f.src} alt={f.tc} />
                      {:else}
                        <span class="blank">{rows === "error" || (row && !f?.src) ? "—" : ""}</span>
                      {/if}
                      <span class="role">{roleLabel(role, e, k)}</span>
                    </button>
                  {/each}
                  {#if strip.more > 0}<span class="more mono">+{strip.more}</span>{/if}
                </div>
              {/each}
            </div>

            <span class="score mono" title={scoreTitle(e)}>{score(e)}</span>
            <span class="verdict">
              <button class="v-btn ok" class:set={e.review === "accept"} title="Accept (y)" onclick={(ev) => { ev.stopPropagation(); review(e, "accept"); }}>✓</button>
              <button class="v-btn no" class:set={e.review === "reject"} title="Reject (x) — left out of the EDL" onclick={(ev) => { ev.stopPropagation(); review(e, "reject"); }}>✗</button>
            </span>
          </div>
        {/each}
      </div>
    </div>
  {/if}
</div>

<style>
  .list {
    flex: 1;
    overflow-y: auto;
    overflow-x: auto;
    min-height: 0;
  }
  .spacer { position: relative; }
  .window { position: absolute; top: 0; left: 0; right: 0; }

  .row {
    display: flex;
    align-items: center;
    gap: var(--gap-md);
    padding: 0 var(--gap-md);
    box-sizing: border-box;
    border-bottom: 1px solid var(--border-subtle);
    cursor: pointer;
    user-select: none;
  }
  .row:hover { background: var(--bg-hover); }
  /* selected = the event you picked; it stays grey while you shuttle
     around it, and turns accent again whenever the cursor is on it */
  .row.selected { background: var(--bg-active); }
  .row.active { background: var(--accent-dim); }
  .row.seen .tc { color: var(--text-secondary); }
  .row.rejected .tc, .row.rejected .desc, .row.rejected .strips { text-decoration: line-through; opacity: 0.5; }
  .row.accepted .tc { color: var(--success); }

  .dot { width: 8px; height: 8px; border-radius: 50%; flex-shrink: 0; }
  .dot.faded { opacity: 0.45; }

  .ident {
    display: flex;
    flex-direction: column;
    gap: 2px;
    width: 190px;
    flex-shrink: 0;
    min-width: 0;
  }
  .tc { font-size: 12px; }
  .seen-mark { color: var(--text-muted); }
  .desc {
    color: var(--text-secondary);
    font-size: 11px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .strips {
    display: flex;
    gap: var(--gap-md);
    flex: 1;
    min-width: 0;
  }
  .strips.loading .tile { opacity: 0.6; }
  .strip { display: flex; align-items: center; gap: 4px; }
  .file-label {
    font-size: 10px;
    font-weight: 600;
    color: var(--text-muted);
    width: 28px;
    flex-shrink: 0;
  }
  /* Every tile is the same box whether it holds an image, a dash or
     nothing yet, so rows never reflow as strips arrive. */
  .tile {
    position: relative;
    width: 96px;
    height: 54px;
    padding: 0;
    flex-shrink: 0;
    background: var(--bg-raised);
    border: 1px solid var(--border-subtle);
    border-radius: 2px;
    overflow: hidden;
    cursor: pointer;
  }
  .tile:disabled { cursor: default; }
  .tile img { width: 100%; height: 100%; object-fit: contain; background: #000; display: block; }
  .tile .blank {
    display: flex;
    width: 100%;
    height: 100%;
    align-items: center;
    justify-content: center;
    color: var(--text-muted);
    font-size: 11px;
  }
  .tile .role {
    position: absolute;
    left: 2px;
    bottom: 1px;
    font-size: 9px;
    line-height: 11px;
    padding: 0 3px;
    border-radius: 2px;
    background: rgba(0, 0, 0, 0.6);
    color: var(--text-secondary);
    pointer-events: none;
  }
  .tile.run { border-color: var(--warning); }
  .tile.run .role { color: var(--warning); }
  .tile.ref { border-style: dashed; cursor: default; }
  .tile.at-cursor { outline: 1px solid var(--text-primary); }
  .more { font-size: 11px; color: var(--text-muted); }

  .score { color: var(--text-muted); font-size: 11px; flex-shrink: 0; width: 44px; text-align: right; }
  .verdict { display: flex; gap: 2px; flex-shrink: 0; }
  .v-btn {
    width: 20px;
    height: 20px;
    padding: 0;
    font-size: 11px;
    line-height: 1;
    background: transparent;
    border: 1px solid transparent;
    border-radius: var(--radius-sm);
    color: var(--text-muted);
    opacity: 0.35;
  }
  .row:hover .v-btn, .row.selected .v-btn, .v-btn.set { opacity: 1; }
  .v-btn.ok.set { color: var(--success); border-color: var(--success); }
  .v-btn.no.set { color: var(--threshold-color); border-color: var(--threshold-color); }
  .v-btn:hover { background: var(--bg-hover); }

  .empty { padding: var(--gap-md); color: var(--text-muted); font-size: 12px; }
</style>
