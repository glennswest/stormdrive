<script>
  // Drive worker jobs (#38): open ones first; each drive's state, step,
  // progress and error; cancel what has not started, resume what a restart
  // interrupted.
  import { post } from './lib/api.js'
  import { sortJobs, countsLine, driveLine, describeStep, canCancel, canResume, jobOpen } from './lib/worker.js'

  let { jobs = [], act, writable = true } = $props()
  const shown = $derived(sortJobs(jobs))
  const open = $derived(jobs.filter(jobOpen).length)
  const time = (t) => (t?.secs_since_epoch ? new Date(t.secs_since_epoch * 1000).toLocaleString() : '')

  function cancel(j) {
    if (confirm(`Cancel job ${j.id}? Steps already running finish; queued ones do not start.`)) act(post(`api/v1/worker/jobs/${j.id}/cancel`))
  }
  function resume(j) {
    if (confirm(`Resume job ${j.id}? Interrupted drives run their next step again.`)) act(post(`api/v1/worker/jobs/${j.id}/resume`))
  }
</script>

<details class="jobs" open={open > 0}>
  <summary>Jobs ({jobs.length}{open ? `, ${open} open` : ''})</summary>
  {#each shown as j (j.id)}
    <details class="job" open={jobOpen(j)}>
      <summary>
        <span class="mono">{j.id}</span>
        <span>{(j.steps || []).map(describeStep).join(' → ')}</span>
        <span class="dim">{countsLine(j)}</span>
        <span class="dim small">{time(j.created)}{j.requester ? ` · ${j.requester.who}` : ''}{j.operation ? ` · ${j.operation}` : ''}</span>
      </summary>
      <div class="row">
        <button disabled={!writable || !canCancel(j)} onclick={() => cancel(j)}>Cancel</button>
        <button disabled={!writable || !canResume(j)} onclick={() => resume(j)}>Resume</button>
      </div>
      {#each j.drives || [] as dj (dj.drive)}
        <div class="dj {dj.state}">
          <span class="mono">{dj.name}</span>
          <span class="mono dim">{dj.serial}</span>
          <span>{driveLine(dj, j.steps)}</span>
          {#if dj.error}<span class="err">{dj.error}</span>{/if}
        </div>
      {/each}
    </details>
  {:else}
    <div class="dim small">no jobs</div>
  {/each}
</details>

<style>
  details.jobs { margin-top: 12px; background: var(--panel); border: 1px solid var(--border); border-radius: var(--radius); padding: 8px 12px; }
  details.jobs > summary { cursor: pointer; color: var(--text-dim); font-size: 12px; text-transform: uppercase; letter-spacing: 0.5px; }
  .job { border-top: 1px solid var(--panel-raised); padding: 6px 0; }
  .job > summary { cursor: pointer; display: flex; gap: 10px; flex-wrap: wrap; font-size: 12px; }
  .row { display: flex; gap: 6px; margin: 6px 0; }
  .dj { display: grid; grid-template-columns: 90px 140px 1fr; gap: 8px; font-size: 12px; padding: 1px 0; }
  .dj .err { grid-column: 2 / -1; color: var(--error); }
  .dj.failed span:nth-child(3), .dj.refused span:nth-child(3) { color: var(--error); }
  .dj.running span:nth-child(3) { color: var(--accent); }
  .dj.interrupted span:nth-child(3) { color: var(--warn); }
  .mono { font-family: var(--mono); }
  .dim { color: var(--text-dim); }
  .small { font-size: 11px; }
</style>
