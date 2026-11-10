<!--
  TimelineSetup — the timeline mode's setup: connect to Resolve, tick the
  video tracks to sample, start.

  The run samples the first, middle and last frame of every clip on the
  ticked tracks, parks Resolve's playhead on each, takes the picture off the
  SDI loop (scopetap_grab) and compares it with the same timecode in the
  file. Resolve's monitoring state is the operator's job: whatever is on the
  SDI output is what gets compared.

  Props:
    filePicked       — a deliverable file is chosen and probed
    grabber          — scopetap_grab path from capabilities (null = missing)
    onStart(opts)    — { tracks, offset, dry_run }
-->
<script lang="ts">
  import { timelineTracks, type TimelineTracksReply } from "../lib/rpc";

  interface Props {
    filePicked: boolean;
    grabber: string | null | undefined;
    onStart: (opts: { tracks: number[]; offset: number; dry_run: boolean }) => void;
  }

  let { filePicked, grabber, onStart }: Props = $props();

  let reply = $state<TimelineTracksReply | null>(null);
  let connecting: boolean = $state(false);
  let connectError: string | null = $state(null);
  let ticked: Set<number> = $state(new Set());
  let offset: number = $state(0);
  let dryRun: boolean = $state(false);
  let showChecklist: boolean = $state(false);

  let grabberPath = $derived(reply?.grabber ?? grabber ?? null);

  async function connect() {
    connecting = true;
    connectError = null;
    try {
      const r = await timelineTracks();
      // Keep the operator's ticks across a refresh when the tracks still exist;
      // otherwise start from the enabled tracks.
      const keep = new Set([...ticked].filter((i) => r.tracks.some((t) => t.index === i)));
      ticked = keep.size > 0
        ? keep
        : new Set(r.tracks.filter((t) => t.enabled !== false && t.items > 0).map((t) => t.index));
      reply = r;
    } catch (e) {
      reply = null;
      connectError = String(e);
    } finally {
      connecting = false;
    }
  }

  function toggle(i: number) {
    const s = new Set(ticked);
    if (s.has(i)) s.delete(i);
    else s.add(i);
    ticked = s;
  }

  let items = $derived(
    reply ? reply.tracks.filter((t) => ticked.has(t.index)).reduce((n, t) => n + t.items, 0) : 0
  );

  let blocker = $derived.by((): string | null => {
    if (!filePicked) return "Choose the deliverable file";
    if (!grabberPath) return "scopetap_grab isn't installed (or set VCOMPARE_GRABBER)";
    if (!reply) return "Connect to Resolve";
    if (ticked.size === 0) return "Tick at least one track";
    if (items === 0) return "The ticked tracks have no clips";
    return null;
  });

  function start() {
    if (blocker) return;
    onStart({ tracks: [...ticked].sort((a, b) => a - b), offset, dry_run: dryRun });
  }
</script>

<div class="tl-setup">
  <div class="row">
    <button onclick={connect} disabled={connecting}>
      {connecting ? "Connecting…" : reply ? "Refresh from Resolve" : "Connect to Resolve"}
    </button>
    {#if reply}
      <span class="mono target" title={reply.target}>
        {reply.target}
        · {reply.timeline.fps ?? "?"} fps{reply.timeline.drop_frame ? " DF" : ""}
        · starts {reply.timeline.start_tc ?? "?"}
      </span>
    {/if}
  </div>
  {#if connectError}
    <div class="err">{connectError}</div>
  {/if}

  {#if reply}
    <div class="tracks">
      {#each [...reply.tracks].reverse() as t (t.index)}
        <label class="track" class:off={t.enabled === false}>
          <input type="checkbox" checked={ticked.has(t.index)} onchange={() => toggle(t.index)} />
          <span class="mono idx">V{t.index}</span>
          <span class="name">{t.name}</span>
          <span class="meta mono">
            {t.items} clip{t.items === 1 ? "" : "s"}{t.enabled === false ? " · track disabled" : ""}
          </span>
        </label>
      {/each}
      {#if reply.tracks.length === 0}
        <div class="meta">This timeline has no video tracks.</div>
      {/if}
    </div>
    <div class="meta">
      {items} clip{items === 1 ? "" : "s"} → up to {items * 3} samples (first, middle and last frame of each)
    </div>
  {/if}

  <div class="row">
    <label title="Positive: the picture is that many frames later in the file than its timecode says">
      Offset
      <input class="num" type="number" bind:value={offset} step="1" /> frames
    </label>
    <label class="checkbox-label" title="Seek and grab every sample but compare nothing and save nothing: measures how long a real run would take">
      <input type="checkbox" bind:checked={dryRun} />
      Dry run (timing only)
    </label>
  </div>

  <div class="row grabber mono" class:missing={!grabberPath}>
    {#if grabberPath}
      grabber: {grabberPath}
    {:else}
      ✕ scopetap_grab not found: install scopetap (or set VCOMPARE_GRABBER) and run scopetap_stream --scopes on the SDI loop
    {/if}
  </div>

  <button class="primary analyze-btn" onclick={start} disabled={blocker != null} title={blocker ?? ""}>
    {dryRun ? "Dry run" : "Check timeline"}
  </button>
  {#if blocker}
    <span class="meta">{blocker}</span>
  {/if}

  <button class="linklike" onclick={() => (showChecklist = !showChecklist)}>
    {showChecklist ? "▾" : "▸"} Before you run: Resolve state that shows up as differences
  </button>
  {#if showChecklist}
    <ul class="checklist">
      <li>Bypass, wipes / split screen, highlight or reference stills on the output</li>
      <li>Timeline proxy or performance mode (Playback › Timeline Proxy Resolution)</li>
      <li>A stale render cache</li>
      <li>LUTs or calibration applied only to the monitor output (Project Settings › Color Management › Lookup Tables)</li>
      <li>Data levels on the SDI output different from the render</li>
    </ul>
  {/if}
</div>

<style>
  .tl-setup {
    display: flex;
    flex-direction: column;
    gap: var(--gap-sm, 8px);
    max-width: 720px;
  }
  .row {
    display: flex;
    align-items: center;
    gap: var(--gap-md, 12px);
    flex-wrap: wrap;
  }
  .row label {
    font-size: 12px;
    color: var(--text-secondary);
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .target {
    color: var(--text-secondary);
    font-size: 11px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 520px;
  }
  .err {
    color: var(--error, #e5534b);
    font-size: 12px;
    white-space: pre-wrap;
  }
  .tracks {
    display: flex;
    flex-direction: column;
    border: 1px solid var(--border-subtle);
    border-radius: 4px;
    padding: 4px 0;
    max-height: 260px;
    overflow-y: auto;
  }
  .track {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 3px 10px;
    cursor: pointer;
    font-size: 12px;
  }
  .track:hover {
    background: var(--bg-hover, rgba(255, 255, 255, 0.04));
  }
  .track.off .name {
    color: var(--text-muted);
  }
  .idx {
    width: 30px;
    color: var(--text-secondary);
  }
  .name {
    flex: 1;
  }
  .meta {
    color: var(--text-muted);
    font-size: 11px;
  }
  .num {
    width: 64px;
  }
  .grabber {
    font-size: 11px;
    color: var(--text-muted);
  }
  .grabber.missing {
    color: var(--warning, #d29922);
  }
  .linklike {
    align-self: flex-start;
    background: none;
    border: none;
    padding: 0;
    color: var(--text-secondary);
    font-size: 11px;
    cursor: pointer;
  }
  .checklist {
    margin: 0;
    padding-left: 18px;
    font-size: 11px;
    color: var(--text-secondary);
  }
</style>
