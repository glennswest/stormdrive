<script>
  // One drive, everything about it: identity, place, health, usage, and
  // every action the grid row has no room for. Confirms before anything
  // destructive; the server re-checks every guard regardless.
  import HealthDot from 'stormview/components/HealthDot.svelte'
  import { post, del } from './lib/api.js'
  import {
    human, shelfKey, healthDot, canJoin, canLeave, canTest, canDestructive,
    canFormat, canFirmware, canForget, canLocate, formatTarget, volumeLine, wearOut,
  } from './lib/model.js'

  // `onprepare(select, title, preset)` opens the Prepare pane (#38): every
  // format goes through the drive worker (restart-safe, destroy guard).
  let { drive: d, images = [], act, onprepare } = $props()
  let image = $state('')

  const l = $derived(d.location || {})
  const h = $derived(d.health || {})
  const u = $derived(d.usage)
  const pct = (done, total) => (total ? Math.floor((100 * done) / total) : 0)

  function place() {
    const p = []
    if (l.shelf) p.push([l.shelf.vendor, l.shelf.model, shelfKey(l.shelf)].filter(Boolean).join(' '))
    if (l.bay != null) p.push(`bay ${l.bay}`)
    if (l.pcie_slot) p.push(`PCIe slot ${l.pcie_slot}`)
    return p.join(' · ') || 'unplaced'
  }

  function join() {
    const n = d.name
    if (confirm(`Join ${n} to the fleet and FORMAT A SLAB on it?\n\nOK = format a slab (destroys data on ${n})\nCancel = choose`))
      return act(post(`api/v1/drives/${d.id}/fleet`, { action: 'join', format_slab: true }))
    if (confirm(`Join ${n} without formatting a slab? (the drive is only opened in stormblock)`))
      return act(post(`api/v1/drives/${d.id}/fleet`, { action: 'join', format_slab: false }))
  }

  async function leave() {
    if (!confirm(`Take ${d.name} out of the fleet?\n\nIf data lives on it, it is drained first and leaves once empty.`)) return
    act(post(`api/v1/drives/${d.id}/fleet`, { action: 'leave', drain: true }))
  }

  function forceLeave() {
    if (confirm(`FORCE ${d.name} out of the fleet without draining? Data on its slabs is stranded.`))
      act(post(`api/v1/drives/${d.id}/fleet`, { action: 'leave', force: true }))
  }

  function test(kind) {
    if (kind === 'destructive_sample' && !confirm(`DESTRUCTIVE test on ${d.name}: writes patterns into sampled regions and destroys data there. Continue?`)) return
    act(post(`api/v1/drives/${d.id}/test`, { kind }))
  }

  function firmware() {
    if (!image) return
    if (!confirm(`Update firmware on ${d.name} (running ${d.firmware}) with ${image}?\n\nThe drive resets once to activate. Fleet drives update one at a time.`)) return
    act(post(`api/v1/drives/${d.id}/firmware`, { image }))
  }

  function overcommit(v) {
    act(post(`api/v1/drives/${d.id}/overcommit`, v === 'off' ? { enabled: false } : { enabled: true, ratio: parseFloat(v) }))
  }

  const ocValues = $derived.by(() => {
    const cur = d.overcommit?.enabled ? String(d.overcommit.ratio) : 'off'
    const vals = ['off', '1.5', '2', '3', '4']
    if (!vals.includes(cur)) vals.push(cur)
    return { cur, vals }
  })
</script>

<h2><HealthDot health={healthDot(d)} /> {d.name}</h2>
<div class="dim">{d.model} · <span class="mono">{d.serial}</span> · fw <span class="mono">{d.firmware}</span></div>
<div class="dim">{place()}</div>

<dl>
  <dt>kind</dt><dd>{d.kind?.replace('_', ' ')}</dd>
  <dt>size</dt><dd>{human(d.capacity_bytes)}</dd>
  <dt>sector</dt><dd>{d.block_size}{d.physical_block_size && d.physical_block_size !== d.block_size ? ` (phys ${d.physical_block_size})` : ''}{d.needs_reformat ? ' — unusable until reformatted' : ''}</dd>
  {#if d.wwid}<dt>wwid</dt><dd class="mono">{d.wwid}</dd>{/if}
  <dt>paths</dt><dd class="mono">{(d.paths || [d.path]).join(' ')}</dd>
  {#if l.controller}<dt>hba</dt><dd class="mono">{l.controller.driver || ''} {l.controller.scsi_host || ''} {l.controller.pcie_addr || ''}</dd>{/if}
  {#if l.sas_address}<dt>sas</dt><dd class="mono">{l.sas_address}{l.sas_phy != null ? ` phy ${l.sas_phy}` : ''}{l.expander ? ` via ${l.expander}` : ''}</dd>{/if}
  {#if l.pcie_addr}<dt>pcie</dt><dd class="mono">{l.pcie_addr}</dd>{/if}
  <dt>id</dt><dd class="mono small">{d.id}</dd>
  {#if d.replaces}<dt>replaces</dt><dd class="mono small">{d.replaces}</dd>{/if}
</dl>

<h3>Health</h3>
<dl>
  <dt>verdict</dt><dd>{h.status || 'unknown'}</dd>
  {#if h.temperature_c != null}<dt>temp</dt><dd>{h.temperature_c} °C</dd>{/if}
  {#if h.wear_pct != null}<dt>wear</dt><dd>{h.wear_pct} %</dd>{/if}
  {#if d.wear_projection}<dt>wear-out</dt><dd>{wearOut(d.wear_projection)}</dd>{/if}
  {#if h.available_spare_pct != null}<dt>spare</dt><dd>{h.available_spare_pct} %</dd>{/if}
  {#if h.power_on_hours != null}<dt>power-on</dt><dd>{h.power_on_hours} h</dd>{/if}
  <dt>{d.kind === 'nvme_ssd' ? 'media errs' : 'io errs'}</dt><dd>{h.media_errors ?? 0}</dd>
</dl>
{#if h.messages?.length}<ul class="msgs">{#each h.messages as m}<li>{m}</li>{/each}</ul>{/if}

{#if u}
  <h3>Usage</h3>
  <dl>
    <dt>used</dt><dd>{human(u.used_bytes)}</dd>
    <dt>free</dt><dd>{human(u.free_bytes)}</dd>
    <dt>outside slabs</dt><dd>{human(u.outside_slabs_bytes)}</dd>
    {#if u.committed_bytes != null}<dt>committed</dt><dd>{human(u.committed_bytes)} of {human(u.promisable_bytes)} promisable</dd>{/if}
    {#if u.headroom_bytes != null}<dt>headroom</dt><dd>{human(u.headroom_bytes)}</dd>{/if}
  </dl>
  {#each u.slabs || [] as s}
    <div class="small dim">{s.role} {s.tier}: {human(s.allocated_bytes)} used of {human(s.total_bytes)}</div>
  {/each}
  {#if u.volumes}
    <!-- #26: absent = the engine reports no placement; [] = none here -->
    <h3>Volumes on this drive ({u.volumes.length})</h3>
    {#each u.volumes.map(volumeLine) as v}
      <div class="vol" class:trouble={v.trouble}>
        <div>{v.name}{#if v.kind} <span class="chip">{v.kind}</span>{/if} <span class="dim">— {v.who}</span></div>
        <div class="small dim">{v.detail}</div>
      </div>
    {:else}
      <div class="small dim">none</div>
    {/each}
  {/if}
{/if}

<h3>Fleet</h3>
<div class="row">
  <span class="chip">{d.membership === 'fleet' ? 'in fleet' : d.in_use_by ? `in use by ${d.in_use_by}` : 'out of fleet'}</span>
  {#if canJoin(d)}<button onclick={join}>Join fleet</button>{/if}
  {#if canLeave(d)}<button onclick={leave}>Leave (drain)</button><button class="danger" onclick={forceLeave}>Force leave</button>{/if}
  {#if canForget(d)}<button onclick={() => confirm(`Forget missing drive ${d.name}? Its record and trend go.`) && act(del(`api/v1/drives/${d.id}`))}>Forget</button>{/if}
</div>
{#if d.drain}
  <div class="small">drain {d.drain.state}: {d.drain.moved} moved, {d.drain.remaining} left{d.drain.failed ? `, ${d.drain.failed} failed` : ''} ({d.drain.reason})
    {#if d.activity === 'draining'}<button onclick={() => act(del(`api/v1/drives/${d.id}/drain`))}>Cancel drain</button>{/if}
  </div>
{/if}
<div class="row">
  <label>designation
    <select value={d.designation} onchange={(e) => act(post(`api/v1/drives/${d.id}/designation`, { designation: e.target.value }))}>
      {#each ['none', 'reserved', 'spare', 'failed'] as v}<option value={v}>{v}</option>{/each}
    </select>
  </label>
  <label title="thin clones on this drive may promise up to ratio × its slab space; stormblock enforces it">overcommit
    <select value={ocValues.cur} onchange={(e) => overcommit(e.target.value)}>
      {#each ocValues.vals as v}<option value={v}>{v === 'off' ? 'off' : v + '×'}</option>{/each}
    </select>
  </label>
</div>

<h3>Activity: {d.activity}</h3>
{#if d.activity === 'testing' && d.test}
  <div class="row"><progress max="100" value={pct(d.test.bytes_done, d.test.bytes_total)}></progress>
    {d.test.kind} {pct(d.test.bytes_done, d.test.bytes_total)}%
    <button onclick={() => act(post(`api/v1/drives/${d.id}/test/cancel`))}>Cancel</button></div>
{/if}
{#if d.prep && d.membership !== 'fleet'}<div class="small">prep: {d.prep.phase}{d.prep.pct != null ? ` ${d.prep.pct}%` : ''}</div>{/if}
{#if ['formatting', 'sanitizing'].includes(d.activity) && !d.format_run && d.prep?.pct != null}
  <div class="row"><progress max="100" value={d.prep.pct}></progress> {d.activity} {d.prep.pct}% (drive worker)</div>
{/if}
{#if d.activity === 'formatting' && d.format_run}
  <div class="row"><progress max="100" value={d.format_run.progress_pct ?? 0}></progress>
    → {d.format_run.to_block_size}: {d.format_run.progress_pct ?? '…'}% {d.format_run.phase}</div>
{/if}
{#if d.activity === 'updating_firmware' && d.firmware_run}
  <div class="row"><progress max="100" value={pct(d.firmware_run.bytes_done, d.firmware_run.bytes_total)}></progress>
    {d.firmware_run.image}: {d.firmware_run.phase}</div>
{/if}
{#if d.test && d.test.state !== 'running'}
  <div class="small">last test: {d.test.kind} {d.test.state}{d.test.errors?.length ? ' — ' + d.test.errors.slice(0, 3).join('; ') : ''}</div>
{/if}
{#if d.format}<div class="small">last format: {d.format.from_block_size} → {d.format.to_block_size} {d.format.state}{d.format.error ? ' — ' + d.format.error : ''}</div>{/if}
{#if d.firmware_update}<div class="small">last firmware: {d.firmware_update.image} {d.firmware_update.from_version} → {d.firmware_update.to_version || '?'} {d.firmware_update.state}{d.firmware_update.reset_required ? ' (activates on next reset)' : ''}{d.firmware_update.error ? ' — ' + d.firmware_update.error : ''}</div>{/if}

<h3>Actions</h3>
<div class="row">
  {#if canTest(d)}
    <button onclick={() => test('smoke')}>Smoke test</button>
    <button onclick={() => test('read_scan')}>Read scan</button>
  {/if}
  {#if canDestructive(d)}<button class="danger" onclick={() => test('destructive_sample')}>Destructive test</button>{/if}
  {#if canLocate(d)}
    <button onclick={() => act(post(`api/v1/drives/${d.id}/locate`, { on: true }))}>💡 Locate</button>
    <button onclick={() => act(post(`api/v1/drives/${d.id}/locate`, { on: false }))}>◦ Off</button>
  {/if}
</div>
{#if canFormat(d) || (d.membership !== 'fleet' && d.activity === 'idle')}
  <div class="row">
    {#if canFormat(d)}<button class="danger" onclick={() => onprepare({ drives: [d.id] }, d.name, { format: formatTarget(d) })}>Format → {formatTarget(d)}…</button>{/if}
    <button onclick={() => onprepare({ drives: [d.id] }, d.name)}>Prepare…</button>
  </div>
{/if}
{#if canFirmware(d)}
  <div class="row">
    <select bind:value={image}>
      <option value="">firmware image…</option>
      {#each images as i}<option value={i.name}>{i.name}</option>{/each}
    </select>
    <button class="danger" disabled={!image} onclick={firmware}>Update firmware</button>
  </div>
{/if}

<style>
  h2 { font-size: 16px; margin: 0 0 4px; display: flex; align-items: center; gap: 8px; }
  h3 { font-size: 11px; text-transform: uppercase; letter-spacing: 0.5px; color: var(--text-faint); margin: 16px 0 6px; }
  dl { display: grid; grid-template-columns: 110px 1fr; gap: 3px 10px; margin: 8px 0; }
  dt { color: var(--text-faint); }
  dd { margin: 0; word-break: break-all; }
  .dim { color: var(--text-dim); }
  .mono { font-family: var(--mono); }
  .small { font-size: 12px; }
  .row { display: flex; gap: 6px; flex-wrap: wrap; align-items: center; margin: 6px 0; }
  .chip { padding: 1px 8px; border-radius: 9px; background: var(--accent-bg); color: var(--accent); font-size: 12px; }
  .vol { margin: 4px 0; }
  .vol.trouble { color: var(--warn); }
  .msgs { margin: 4px 0; padding-left: 18px; color: var(--warn); font-size: 12px; }
  button.danger { color: var(--error); }
  progress { width: 140px; }
  label { display: inline-flex; gap: 6px; align-items: center; color: var(--text-dim); }
</style>
