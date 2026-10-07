<script>
  // Prepare drives with the drive worker (#38): pick steps, preview them as
  // a dry run (which drives run, which are refused and why), type the serial
  // of every drive that holds data before it may be destroyed, then submit.
  // The dry run is a read; the submit needs a storage-admin (#45/#47).
  import { post } from './lib/api.js'
  import {
    SANITIZE, SECURITY_ERASE, TESTS, blankForm, stepsOf, describeStep, destroys, requestOf, heldDrives, namesDrive, destroyList,
  } from './lib/worker.js'

  let { select, title, preset = {}, drives = [], writable = true, ondone, onerror } = $props()

  let form = $state(blankForm(preset))
  let plan = $state(null)
  let planned = $state('') // the request the plan is for
  let typed = $state({})
  let busy = $state(false)

  const byId = $derived(new Map(drives.map((d) => [d.id, d])))
  const steps = $derived(stepsOf(form))
  const key = $derived(JSON.stringify({ select, steps }))
  const fresh = $derived(!!plan && planned === key)
  const held = $derived(fresh ? heldDrives(plan, byId) : [])
  const destroy = $derived(destroyList(held, typed))
  const others = $derived(fresh ? plan.refused.filter((r) => !held.some((h) => h.drive === r.drive)) : [])
  const total = $derived(fresh ? plan.runnable + destroy.length : 0)

  async function preview() {
    busy = true
    try {
      plan = await post('api/v1/worker/jobs', requestOf(select, form, [], true))
      planned = key
      typed = {}
    } catch (e) {
      plan = null
      onerror?.(e.message || String(e))
    } finally {
      busy = false
    }
  }

  async function submit() {
    if (!fresh || !total) return
    const names = [...(plan.drives || []).map((d) => d.name), ...held.filter((h) => destroy.includes(h.serial)).map((h) => h.name)]
    const warn = destroys(steps) ? `\n\nThis DESTROYS everything on ${total} drive(s)${destroy.length ? `, ${destroy.length} of them holding data you named` : ''}.` : ''
    if (!confirm(`Run ${steps.map(describeStep).join(' → ')} on ${total} drive(s)?\n\n${names.join(', ')}${warn}`)) return
    busy = true
    try {
      const job = await post('api/v1/worker/jobs', requestOf(select, form, destroy, false))
      ondone?.(job)
    } catch (e) {
      onerror?.(e.message || String(e))
    } finally {
      busy = false
    }
  }
</script>

<h2>Prepare</h2>
<div class="dim">{title}</div>

<fieldset class="steps" disabled={busy}>
  <label>Format
    <select bind:value={form.format}>
      <option value={0}>—</option>
      <option value={4096}>→ 4096</option>
      <option value={512}>→ 512</option>
    </select>
  </label>
  <label>Sanitize
    <select bind:value={form.sanitize}>
      <option value="">—</option>
      {#each SANITIZE as m}<option value={m}>{m}</option>{/each}
      {#each SECURITY_ERASE as [v, label]}<option value={v}>{label}</option>{/each}
    </select>
  </label>
  <label>Test
    <select bind:value={form.test}>
      <option value="">—</option>
      {#each TESTS as t}<option value={t}>{t.replace('_', ' ')}</option>{/each}
    </select>
  </label>
  <label><input type="checkbox" bind:checked={form.partition} /> Partition (GPT, one slab partition)</label>
  <label><input type="checkbox" bind:checked={form.enroll} /> Enroll in stormblock</label>
  {#if form.enroll}
    <label>Tier <input type="text" placeholder="from the drive kind" bind:value={form.tier} /></label>
  {/if}
  {#if form.partition || form.enroll}
    <label>Role
      <select bind:value={form.role}>
        <option value="data">data</option>
        <option value="system">system</option>
      </select>
    </label>
  {/if}
</fieldset>

<div class="row">
  <span class="mono small">{steps.length ? steps.map(describeStep).join(' → ') : 'no steps'}</span>
</div>
<div class="row">
  <button disabled={busy || !steps.length} onclick={preview}>Preview (dry run)</button>
  <button class="danger" disabled={busy || !fresh || !total || !writable} onclick={submit}
    title={writable ? '' : 'needs storage-admin: sign in'}>Run on {total || '…'} drive{total === 1 ? '' : 's'}</button>
</div>
{#if !writable}<div class="small warn">needs storage-admin: sign in to run (the preview is open)</div>{/if}

{#if plan && !fresh}
  <div class="small dim">the steps changed — preview again</div>
{:else if fresh}
  <h3>Runs on {plan.runnable}</h3>
  <div class="names">{(plan.drives || []).map((d) => d.name).join(', ') || '—'}</div>

  {#if held.length}
    <h3>Holds data ({held.length})</h3>
    <div class="small dim">Type each drive's serial to let these steps destroy what it holds.</div>
    {#each held as h (h.drive)}
      <label class="held" class:ok={namesDrive(typed[h.drive], h)}>
        <span class="mono">{h.name}</span>
        <span class="small">{h.what}</span>
        <input type="text" placeholder={`type ${h.serial || 'its serial'}`} autocomplete="off" spellcheck="false"
          value={typed[h.drive] || ''} oninput={(e) => (typed = { ...typed, [h.drive]: e.target.value })} />
      </label>
    {/each}
  {/if}

  {#if others.length}
    <h3>Refused ({others.length})</h3>
    {#each others as r (r.drive)}
      <div class="small"><span class="mono">{r.name}</span> — {r.reason}</div>
    {/each}
  {/if}
{/if}

<style>
  h2 { margin: 0 0 2px; font-size: 15px; }
  h3 { margin: 14px 0 4px; font-size: 12px; color: var(--text-dim); text-transform: uppercase; letter-spacing: 0.5px; }
  .dim { color: var(--text-dim); }
  .small { font-size: 12px; }
  .warn { color: var(--warn); }
  .mono { font-family: var(--mono); }
  fieldset.steps { border: 1px solid var(--border); border-radius: var(--radius-sm); margin: 10px 0; padding: 8px 10px;
    display: flex; flex-direction: column; gap: 6px; }
  fieldset.steps label { display: flex; gap: 8px; align-items: center; }
  .row { display: flex; gap: 6px; flex-wrap: wrap; align-items: center; margin: 6px 0; }
  button.danger { color: var(--error); }
  .names { font-family: var(--mono); font-size: 12px; word-break: break-word; }
  .held { display: grid; grid-template-columns: 80px 1fr 150px; gap: 6px; align-items: center; margin: 3px 0; }
  .held.ok input { border-color: var(--ok, var(--accent)); }
</style>
