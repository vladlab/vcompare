<!--
  TimelineReport — what a timeline run looked at and how it went: the
  Resolve timeline and tracks, the SDI signal against the file's tags,
  samples by status, timing, and any warnings. Shown above the results, and
  on its own for a dry run (which has no differences to chart).
-->
<script lang="ts">
  import type { TimelineRunReport } from "../lib/rpc";

  interface Props {
    report: TimelineRunReport;
    /** Show the full diagnostics, not just warnings (dry runs) */
    full?: boolean;
  }

  let { report, full = false }: Props = $props();

  let showDiag: boolean = $state(false);
  $effect(() => {
    showDiag = full;
  });

  const STATUS_LABELS: Record<string, string> = {
    ok: "compared",
    grabbed: "grabbed",
    not_in_file: "outside the file",
    grab_timeout: "never settled",
    grab_error: "grab failed",
    seek_error: "seek refused",
    compare_error: "compare failed",
    wrong_tc: "wrong timecode",
  };
  const STATUS_ORDER = ["ok", "grabbed", "not_in_file", "grab_timeout", "grab_error", "seek_error", "compare_error", "wrong_tc"];

  let counts = $derived(
    Object.entries(report.counts ?? {})
      .sort((a, b) => STATUS_ORDER.indexOf(a[0]) - STATUS_ORDER.indexOf(b[0]))
  );
  let warnings = $derived((report.diagnostics ?? []).filter((l) => l.startsWith("warning:")).map((l) => l.slice(8).trim()));

  function fileSignal(): string {
    const f = report.file_signal;
    if (!f) return "?";
    return `${f.width}×${f.height} ${f.pix_fmt}, ${f.matrix}${f.matrix_tagged ? "" : "?"} ${f.full_range ? "full" : "limited"}${f.range_tagged ? "" : "?"}${f.transfer ? `, ${f.transfer}` : ""}`;
  }

  function fmtSecs(s: number): string {
    if (s < 90) return `${s.toFixed(1)} s`;
    const m = Math.floor(s / 60);
    return m < 90 ? `${m} min ${Math.round(s % 60)} s` : `${Math.floor(m / 60)} h ${m % 60} min`;
  }
</script>

<div class="report">
  <div class="line">
    <span class="k">Resolve</span>
    <span class="mono">{report.resolve?.target ?? "?"}</span>
    <span class="muted mono">
      · V{(report.resolve?.tracks ?? []).join(", V")}
      {#if report.offset}· offset {report.offset > 0 ? "+" : ""}{report.offset}{/if}
    </span>
  </div>
  <div class="line">
    <span class="k">SDI</span>
    {#if report.signal}
      <span class="mono">
        {report.signal.width}×{report.signal.height} {report.signal.name}, levels {report.signal.levels}{report.signal.levels_auto ? " (auto)" : ""}
      </span>
    {:else}
      <span class="muted">no frame grabbed</span>
    {/if}
  </div>
  <div class="line">
    <span class="k">File</span>
    <span class="mono" title="? = untagged, assumed">{fileSignal()}</span>
  </div>
  <div class="line">
    <span class="k">Samples</span>
    <span class="mono">
      {#each counts as [k, n], i}
        {#if i > 0} · {/if}<span class:warn={!["ok", "grabbed", "not_in_file"].includes(k)}>{n} {STATUS_LABELS[k] ?? k}</span>
      {/each}
    </span>
  </div>
  {#if report.timing}
    <div class="line">
      <span class="k">Time</span>
      <span class="mono">
        {fmtSecs(report.timing.elapsed_s)} · {report.timing.per_sample_ms} ms/sample · grab wait
        median {report.timing.grab_wait_ms.median} / p95 {report.timing.grab_wait_ms.p95} / max {report.timing.grab_wait_ms.max} ms
      </span>
    </div>
  {/if}
  {#each warnings as w}
    <div class="warning">⚠ {w}</div>
  {/each}
  {#if (report.diagnostics ?? []).length > 0}
    <button class="linklike" onclick={() => (showDiag = !showDiag)}>{showDiag ? "▾" : "▸"} diagnostics</button>
    {#if showDiag}
      <pre class="diag mono">{(report.diagnostics ?? []).join("\n")}</pre>
    {/if}
  {/if}
</div>

<style>
  .report {
    display: flex;
    flex-direction: column;
    gap: 3px;
    padding: 6px var(--gap-lg, 16px);
    background: var(--bg-surface);
    border-bottom: 1px solid var(--border-subtle);
    font-size: 11px;
    color: var(--text-secondary);
  }
  .line {
    display: flex;
    gap: 8px;
    align-items: baseline;
  }
  .line > :not(.k) {
    min-width: 0;
  }
  .k {
    color: var(--text-muted);
    width: 52px;
    flex-shrink: 0;
  }
  .muted {
    color: var(--text-muted);
  }
  .warn {
    color: var(--warning, #d29922);
  }
  .warning {
    color: var(--warning, #d29922);
  }
  .linklike {
    align-self: flex-start;
    background: none;
    border: none;
    padding: 0;
    color: var(--text-muted);
    font-size: 11px;
    cursor: pointer;
  }
  .diag {
    margin: 0;
    max-height: 240px;
    overflow: auto;
    font-size: 11px;
    white-space: pre-wrap;
  }
</style>
