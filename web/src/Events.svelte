<script>
  // The newest events first: what happened to which drive, when.
  let { events = [] } = $props()
  const shown = $derived(events.slice(-100).reverse())
  const time = (e) => (e.time?.secs_since_epoch ? new Date(e.time.secs_since_epoch * 1000).toLocaleString() : '')
</script>

<details open>
  <summary>Events ({events.length})</summary>
  <div class="list">
    {#each shown as e (e.seq)}
      <div class="ev">
        <span class="t">{time(e)}</span>
        <span class="sev {e.severity}">{e.severity}</span>
        <span class="k">{e.kind}</span>
        <span>{e.message}</span>
      </div>
    {:else}
      <div class="t">no events</div>
    {/each}
  </div>
</details>

<style>
  details { margin-top: 12px; background: var(--panel); border: 1px solid var(--border); border-radius: var(--radius); padding: 8px 12px; }
  summary { cursor: pointer; color: var(--text-dim); font-size: 12px; text-transform: uppercase; letter-spacing: 0.5px; }
  .list { max-height: 260px; overflow-y: auto; margin-top: 6px; }
  .ev { display: flex; gap: 8px; font-size: 12px; padding: 2px 0; border-bottom: 1px solid var(--panel-raised); }
  .t { color: var(--text-faint); font-family: var(--mono); font-size: 11px; white-space: nowrap; }
  .k { color: var(--text-dim); }
  .sev.warning { color: var(--warn); }
  .sev.error { color: var(--error); }
  .sev.info { color: var(--text-faint); }
</style>
