<script>
  // A shelf (SES report: identity, paths, PSU/fan/temperature/voltage
  // elements, shelf-wide actions) or an HBA (the firmware it runs).
  import HealthDot from 'stormview/components/HealthDot.svelte'
  import { post } from './lib/api.js'
  import { sesDot, iomLine, psuLine, connectorLine, sensorElement } from './lib/model.js'

  let { group: g, act, onprepare } = $props()
  const r = $derived(g.report)
  const needs = $derived(g.drives.filter((d) => d.needs_reformat && d.membership !== 'fleet').length)
  const sm = $derived(r?.summary)
  const elements = $derived((r?.elements || []).filter(sensorElement))

  function value(e) {
    if (e.temperature_c != null) return `${e.temperature_c} °C`
    if (e.rpm != null) return `${e.rpm} rpm`
    if (e.volts != null) return `${e.volts.toFixed(2)} V`
    if (e.amps != null) return `${e.amps.toFixed(2)} A`
    return ''
  }

  // Shelf-wide work goes through the drive worker's Prepare pane (#38).
  const reformat = () => onprepare({ shelf: g.key, unusable: true }, `${g.label}: drives the kernel cannot use`, { format: 4096 })
</script>

<h2>{g.label}</h2>
<div class="dim">{g.drives.length} of {g.all} drives shown</div>

{#if g.kind === 'shelf'}
  {#if r}
    <dl>
      <dt>status</dt><dd><HealthDot health={sesDot(r.status)} /> {r.status}</dd>
      <dt>logical id</dt><dd class="mono">{r.key}</dd>
      {#if sm?.shelf_id}<dt>shelf ID</dt><dd class="mono">{sm.shelf_id}</dd>{/if}
      {#if sm?.serial || r.shelf.serial}<dt>serial</dt><dd class="mono">{sm?.serial || r.shelf.serial}</dd>{/if}
      {#if sm?.part_number}<dt>part</dt><dd class="mono">{sm.part_number}</dd>{/if}
      <dt>paths</dt><dd>{r.paths} {#each r.esps || [] as p}<span class="mono small"> {p.scsi_id}</span>{/each}</dd>
      {#if sm?.multipath}<dt>multipath</dt><dd class="small">{sm.multipath.note}</dd>{/if}
      <dt>PSU</dt><dd>{r.power_supplies.ok}/{r.power_supplies.total}</dd>
      <dt>fans</dt><dd>{r.fans.ok}/{r.fans.total}</dd>
      <dt>slots</dt><dd>{r.slots.ok}/{r.slots.total}</dd>
      {#if r.max_temperature_c != null}<dt>hottest</dt><dd>{r.max_temperature_c} °C</dd>{/if}
    </dl>
    <div class="row">
      <button onclick={() => act(post(`api/v1/shelves/${g.key}/locate`, { on: true }))}>💡 Locate shelf</button>
      <button onclick={() => act(post(`api/v1/shelves/${g.key}/locate`, { on: false }))}>◦ Off</button>
      <button class="danger" disabled={!needs} onclick={reformat}>Reformat {needs || ''} → 4096…</button>
      <button onclick={() => onprepare({ shelf: g.key }, g.label)}>Prepare shelf…</button>
    </div>
    {#if sm?.problems?.length || r.help_text}
      <h3>Problems</h3>
      {#each sm?.problems || [] as p}
        <div class="el">
          <HealthDot health={sesDot(p.status)} size={8} />
          <span>{p.element}</span>
          <span>{p.status}</span>
          <span class="dim small">{(p.flags || []).join(', ')}</span>
          {#if p.sas_address}<span class="mono small">{p.sas_address}</span>{/if}
        </div>
      {/each}
      {#if r.help_text}<div class="small">The shelf says: {r.help_text}</div>{/if}
    {/if}
    {#if sm?.ioms?.length}
      <h3>IOMs</h3>
      {#each sm.ioms as i}
        <div class="el"><HealthDot health={i.installed ? sesDot(i.status) : 'unknown'} size={8} /><span>{iomLine(i)}</span></div>
      {/each}
    {/if}
    {#if sm?.power_supplies?.length}
      <h3>Power</h3>
      {#each sm.power_supplies as p}
        <div class="el"><HealthDot health={p.installed ? sesDot(p.status) : 'unknown'} size={8} /><span>{psuLine(p)}</span></div>
      {/each}
    {/if}
    {#if sm?.connectors?.some((c) => c.installed)}
      <h3>SAS ports</h3>
      {#each sm.connectors.filter((c) => c.installed) as c}
        <div class="el"><HealthDot health={sesDot(c.status)} size={8} /><span>{connectorLine(c)}</span></div>
      {/each}
    {/if}
    <h3>Sensors</h3>
    {#each elements as e}
      <div class="el">
        <HealthDot health={sesDot(e.status)} size={8} />
        <span>{e.name || `${e.type_name} ${e.index}`}</span>
        <span class="mono">{value(e)}</span>
        <span class="dim small">{(e.flags || []).join(', ')}</span>
        {#if e.ident}💡{/if}
      </div>
    {:else}
      <div class="dim">no element detail</div>
    {/each}
  {:else}
    <div class="dim">No SES report for this shelf: its drives name it through sysfs, but no enclosure device answered.</div>
  {/if}
{:else if g.kind === 'hba'}
  {@const h = g.hba || {}}
  <dl>
    <dt>pcie</dt><dd class="mono">{g.key}</dd>
    <dt>driver</dt><dd>{h.driver || g.controller?.driver || '?'}</dd>
    {#if h.board_name}<dt>board</dt><dd>{h.board_name}</dd>{/if}
    {#if h.firmware}<dt>firmware</dt><dd class="mono">{h.firmware}</dd>{/if}
    {#if h.bios}<dt>bios</dt><dd class="mono">{h.bios}</dd>{/if}
    {#if h.nvdata}<dt>nvdata</dt><dd class="mono">{h.nvdata}</dd>{/if}
    {#if h.pci_id}<dt>pci id</dt><dd class="mono">{h.pci_id}</dd>{/if}
    {#if h.sas_address}<dt>sas</dt><dd class="mono">{h.sas_address}</dd>{/if}
    {#if h.scsi_hosts?.length}<dt>hosts</dt><dd class="mono">{h.scsi_hosts.join(' ')}</dd>{/if}
  </dl>
  <div class="dim small">Reported only: stormdrive never flashes an HBA.</div>
{/if}

<style>
  h2 { font-size: 16px; margin: 0 0 4px; }
  h3 { font-size: 11px; text-transform: uppercase; letter-spacing: 0.5px; color: var(--text-faint); margin: 16px 0 6px; }
  dl { display: grid; grid-template-columns: 90px 1fr; gap: 3px 10px; margin: 8px 0; }
  dt { color: var(--text-faint); }
  dd { margin: 0; word-break: break-all; display: flex; gap: 6px; align-items: center; flex-wrap: wrap; }
  .dim { color: var(--text-dim); }
  .mono { font-family: var(--mono); }
  .small { font-size: 12px; }
  .row { display: flex; gap: 6px; flex-wrap: wrap; margin: 8px 0; }
  .el { display: flex; gap: 8px; align-items: center; font-size: 12px; padding: 2px 0; }
  button.danger { color: var(--error); }
</style>
