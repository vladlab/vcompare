<!--
  TabStrip — the tabs under the chart. Detector tabs carry a count badge;
  a tab that exists for the mode but can't run (old cache without the data
  it needs) is shown greyed with the reason as its tooltip.
-->
<script lang="ts">
  import type { TabId, TabItem } from "../lib/detectors";

  interface Props {
    tabs: TabItem[];
    active: TabId;
    onSelect: (id: TabId) => void;
  }

  let { tabs, active, onSelect }: Props = $props();
</script>

<div class="tab-strip" role="tablist">
  {#each tabs as t, i (t.id)}
    <button
      role="tab"
      class="tab"
      class:active={t.id === active}
      class:unavailable={!!t.unavailable}
      aria-selected={t.id === active}
      title={t.unavailable ?? `${t.label} (${i + 1})`}
      onclick={(e) => { onSelect(t.id); (e.currentTarget as HTMLElement).blur(); }}
    >
      {t.label}
      {#if t.count != null && !t.unavailable}
        <span class="badge mono" class:zero={t.count === 0}>{t.count}</span>
      {/if}
    </button>
  {/each}
</div>

<style>
  .tab-strip {
    display: flex;
    gap: 2px;
    padding: 0 var(--gap-md);
    border-bottom: 1px solid var(--border-subtle);
    background: var(--bg-surface);
    flex-shrink: 0;
  }
  .tab {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 6px 12px;
    background: transparent;
    border: none;
    border-bottom: 2px solid transparent;
    border-radius: 0;
    color: var(--text-secondary);
    font-size: 12px;
    cursor: pointer;
  }
  .tab:hover { color: var(--text-primary); background: var(--bg-hover); }
  .tab.active { color: var(--text-primary); border-bottom-color: var(--warning); }
  .tab.unavailable { opacity: 0.45; }
  .badge {
    font-size: 10px;
    padding: 0 5px;
    border-radius: 8px;
    background: var(--warning);
    color: #000;
    font-weight: 600;
  }
  .badge.zero { background: var(--bg-raised); color: var(--text-muted); }
</style>
