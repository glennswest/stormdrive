<script>
  // The stormdrive page (#6): built for hundreds of drives. Groups (shelf,
  // HBA, NVMe, unlocated) are the top rows of a stormview DataGrid and
  // each group's drives are a nested grid, so a collapsed shelf costs one
  // row. Rows are keyed by id, so a 4 s refresh updates cells instead of
  // rebuilding the table. Anything richer than a cell lives in the side
  // pane (click a row); anything for many drives lives in the bulk bar
  // (tick rows — ticking a group means every drive it shows). Formats and
  // the rest of drive preparation go through the drive worker (Prepare
  // pane, Jobs panel, #38). Writes need a storage-admin bearer (#47).
  import DataGrid from 'stormview/components/DataGrid.svelte'
  import { get, post, each, bearer, setBearer } from './lib/api.js'
  import {
    QUICK, groupDrives, selectedDrives, healthDot, sesDot, human,
    canFirmware, canTest, canLocate, needsAttention,
  } from './lib/model.js'
  import { prepMetric } from './lib/worker.js'
  import DrivePane from './DrivePane.svelte'
  import GroupPane from './GroupPane.svelte'
  import Prepare from './Prepare.svelte'
  import Jobs from './Jobs.svelte'
  import Firmware from './Firmware.svelte'
  import Events from './Events.svelte'

  let node = $state(null)
  let drives = $state([])
  let shelves = $state([])
  let hbas = $state([])
  let events = $state([])
  let images = $state([])
  let error = $state('')
  let notice = $state('')
  let quick = $state('all')
  let text = $state('')
  let selected = $state([])
  let pane = $state(null) // {type: 'drive'|'group', id}
  let image = $state('')
  let working = $state(false)
  let jobs = $state([])
  let token = $state(bearer())
  let signingIn = $state(false)
  let pasted = $state('')

  // The node's gate (`/api/v1/health` → writes.gate): while it enforces and
  // no bearer is set, write controls are disabled. The server decides
  // anyway; a 401/403 says "needs storage-admin".
  const enforcing = $derived(node?.writes?.gate === 'enforce')
  const writable = $derived(!enforcing || !!token)

  function signIn() {
    setBearer(pasted)
    token = bearer()
    pasted = ''
    signingIn = false
  }
  function signOut() {
    setBearer('')
    token = ''
  }

  /// Open the Prepare pane on a worker selection.
  function prepare(select, title, preset = {}) {
    pane = { type: 'prepare', select, title, preset, n: Date.now() }
  }

  const groups = $derived(groupDrives(drives, shelves, hbas, { quick, text }))
  const picked = $derived(selectedDrives(selected, groups))
  const pickedDrives = $derived(drives.filter((d) => picked.has(d.id)))
  const counts = $derived({
    total: drives.length,
    fleet: drives.filter((d) => d.membership === 'fleet').length,
    attention: drives.filter(needsAttention).length,
    reformat: drives.filter((d) => d.needs_reformat).length,
    busy: drives.filter((d) => ['testing', 'formatting', 'sanitizing', 'updating_firmware', 'draining'].includes(d.activity)).length,
  })
  const paneDrive = $derived(pane?.type === 'drive' ? drives.find((d) => d.id === pane.id) : null)
  const paneGroup = $derived(pane?.type === 'group' ? groups.find((g) => g.id === pane.id) || groupDrives(drives, shelves, hbas).find((g) => g.id === pane.id) : null)

  async function refresh() {
    try {
      const [h, dr, sh, ev, im, hb, jb] = await Promise.all([
        get('api/v1/health'),
        get('api/v1/drives'),
        get('api/v1/shelves'),
        get('api/v1/events'),
        get('api/v1/firmware/images').catch(() => ({ images: [] })),
        get('api/v1/hbas').catch(() => ({ hbas: [] })),
        get('api/v1/worker/jobs').catch(() => ({ jobs: [] })),
      ])
      node = h
      drives = dr.drives
      shelves = sh.shelves
      events = ev.events
      images = im.images || []
      hbas = hb.hbas || []
      jobs = jb.jobs || []
      error = ''
    } catch (e) {
      error = e.message || String(e)
    }
  }

  $effect(() => {
    refresh()
    const t = setInterval(() => { if (!document.hidden) refresh() }, 4000)
    return () => clearInterval(t)
  })

  function say(msg) {
    notice = msg
    setTimeout(() => { if (notice === msg) notice = '' }, 10000)
  }

  /// Run an action, show its error, refresh either way.
  async function act(p) {
    try {
      return await p
    } catch (e) {
      error = e.message || String(e)
    } finally {
      refresh()
    }
  }

  // ------------------------------------------------------------ grid rows

  function stateMetrics(d) {
    const m = []
    if (d.membership === 'fleet') m.push({ label: '', value: 'fleet', tone: 'accent' })
    else if (d.in_use_by) m.push({ label: '', value: 'in use', tone: 'accent' })
    else m.push({ label: '', value: 'out', tone: 'muted' })
    if (d.designation !== 'none') m.push({ label: '', value: d.designation, tone: d.designation === 'failed' ? 'error' : 'muted' })
    const pct = d.format_run?.progress_pct ?? d.prep?.pct
    if (d.activity === 'formatting' || d.activity === 'sanitizing') m.push({ label: d.activity === 'formatting' ? 'format' : 'sanitize', value: pct ?? '…', unit: pct != null ? '%' : '', tone: 'warn' })
    else if (d.activity === 'updating_firmware') {
      const r = d.firmware_run || {}
      m.push({ label: 'fw', value: r.bytes_total ? Math.floor((100 * r.bytes_done) / r.bytes_total) : r.phase || '…', unit: r.bytes_total ? '%' : '', tone: 'warn' })
    } else if (d.activity === 'testing' && d.test) {
      const t = d.test
      m.push({ label: t.kind.replace('_', ' '), value: t.bytes_total ? Math.floor((100 * t.bytes_done) / t.bytes_total) : '…', unit: t.bytes_total ? '%' : '', tone: 'accent' })
    } else if (d.activity !== 'idle') m.push({ label: '', value: d.activity, tone: d.activity === 'missing' ? 'error' : 'warn' })
    else if (d.test && d.test.state !== 'running') {
      const t = d.test
      m.push({ label: t.kind.replace('_', ' '), value: t.state, tone: t.state === 'passed' ? 'ok' : t.state === 'failed' ? 'error' : 'muted' })
    }
    const pm = d.activity === 'idle' ? prepMetric(d) : null
    if (pm) m.push(pm)
    if (d.health?.temperature_c != null) m.push({ label: '', value: d.health.temperature_c, unit: '°C', tone: d.health.temperature_c >= 55 ? 'warn' : 'muted' })
    if (d.health?.wear_pct != null) m.push({ label: 'wear', value: d.health.wear_pct, unit: '%', tone: d.health.wear_pct >= 80 ? 'warn' : 'muted' })
    return m
  }

  function driveRow(d) {
    const l = d.location || {}
    const actions = []
    if (canLocate(d)) {
      actions.push({ id: 'locate-on', label: '💡', enabled: writable })
      actions.push({ id: 'locate-off', label: '◦', enabled: writable })
    }
    return {
      id: d.id,
      _drive: true,
      name: d.name + (d.paths?.length > 1 ? ` ×${d.paths.length}` : ''),
      bay: l.bay ?? (l.pcie_slot != null ? Number(l.pcie_slot) || l.pcie_slot : null),
      model: `${d.model || '?'} · ${d.serial || ''}`,
      kind: (d.kind || '').replace('_', ' '),
      firmware: d.firmware,
      capacity_bytes: d.capacity_bytes,
      free_bytes: d.usage ? d.usage.free_bytes : null,
      block_size: d.block_size,
      needs_reformat: !!d.needs_reformat,
      dot: healthDot(d),
      state: stateMetrics(d),
      actions,
    }
  }

  const driveColumns = [
    { key: 'name', label: 'Drive', render: 'mono' },
    { key: 'bay', label: 'Bay', width: '56px' },
    { key: 'model', label: 'Model · serial' },
    { key: 'kind', label: 'Kind' },
    { key: 'firmware', label: 'FW', render: 'mono' },
    { key: 'capacity_bytes', label: 'Size', render: (r) => human(r.capacity_bytes) },
    { key: 'free_bytes', label: 'Free', render: (r) => (r.free_bytes == null ? '' : human(r.free_bytes)) },
    { key: 'block_size', label: 'Sector', render: (r) => (r.needs_reformat ? `${r.block_size} ✗` : String(r.block_size)) },
    { key: 'dot', label: 'Health', render: 'health' },
    { key: 'state', label: 'State', render: 'metrics', sortable: false },
    { key: 'actions', label: '', render: 'actions', sortable: false },
  ]

  function groupRow(g) {
    const r = g.report
    const m = [{ label: 'drives', value: g.drives.length === g.all ? g.all : `${g.drives.length}/${g.all}` }]
    const fleet = g.drives.filter((d) => d.membership === 'fleet').length
    if (fleet) m.push({ label: 'fleet', value: fleet, tone: 'accent' })
    const bad = g.drives.filter(needsAttention).length
    if (bad) m.push({ label: 'attention', value: bad, tone: 'warn' })
    if (r) {
      m.push({ label: 'psu', value: `${r.power_supplies.ok}/${r.power_supplies.total}`, tone: r.power_supplies.ok < r.power_supplies.total ? 'error' : 'muted' })
      m.push({ label: 'fans', value: `${r.fans.ok}/${r.fans.total}`, tone: r.fans.ok < r.fans.total ? 'error' : 'muted' })
      if (r.max_temperature_c != null) m.push({ label: 'temp', value: r.max_temperature_c, unit: '°C' })
      m.push({ label: 'paths', value: r.paths, tone: r.paths > 1 ? 'accent' : 'muted' })
    }
    if (g.hba?.firmware) m.push({ label: 'fw', value: g.hba.firmware, tone: 'muted' })
    const worst = g.drives.map(healthDot)
    let dot = r ? sesDot(r.status) : 'ok'
    if (worst.includes('error')) dot = 'error'
    else if (worst.includes('warn') && dot !== 'error') dot = 'warn'
    if (!g.all && !r) dot = 'idle'
    const actions = []
    if (g.kind === 'shelf' && r) {
      actions.push({ id: 'shelf-locate-on', label: '💡', enabled: writable })
      actions.push({ id: 'shelf-locate-off', label: '◦', enabled: writable })
    }
    return { id: g.id, _group: g, label: g.label, dot, metrics: m, actions }
  }

  const groupColumns = [
    { key: 'label', label: 'Shelf / HBA' },
    { key: 'dot', label: 'Health', render: 'health', width: '90px' },
    { key: 'metrics', label: '', render: 'metrics', sortable: false },
    { key: 'actions', label: '', render: 'actions', sortable: false },
  ]

  const groupRows = $derived(groups.map(groupRow))

  // A drive grid's nested getChildren must be a function (null falls back
  // to the parent's), so drives get one that has no children.
  const none = () => []
  function children(row) {
    if (!row._group) return []
    const g = row._group
    return [{
      title: `${g.drives.length} drive${g.drives.length === 1 ? '' : 's'}`,
      rows: g.drives.map(driveRow),
      columns: driveColumns,
      getChildren: none,
    }]
  }

  function onaction(row, a) {
    if (a.id === 'locate-on' || a.id === 'locate-off') act(post(`api/v1/drives/${row.id}/locate`, { on: a.id === 'locate-on' }))
    if (a.id === 'shelf-locate-on' || a.id === 'shelf-locate-off') act(post(`api/v1/shelves/${row._group.key}/locate`, { on: a.id === 'shelf-locate-on' }))
  }

  function onrowclick(row) {
    pane = row._group ? { type: 'group', id: row.id } : { type: 'drive', id: row.id }
  }

  // ------------------------------------------------------------ bulk bar

  function summary(results, verb) {
    const ok = results.filter((r) => r.ok).length
    const bad = results.filter((r) => !r.ok)
    say(`${verb}: ${ok} ok` + (bad.length ? `, ${bad.length} refused — ` + bad.slice(0, 5).map((r) => `${r.item.name}: ${r.error}`).join('; ') + (bad.length > 5 ? ' …' : '') : ''))
  }

  async function bulk(fn) {
    working = true
    try {
      await fn()
    } catch (e) {
      error = e.message || String(e)
    } finally {
      working = false
      refresh()
    }
  }

  /// The selection, to the Prepare pane: the dry run says which run.
  const bulkPrepare = (preset = {}) =>
    prepare({ drives: pickedDrives.map((d) => d.id) }, `${pickedDrives.length} selected drive(s)`, preset)

  function prepared(job) {
    say(`job ${job.id}: ${job.drives?.filter((d) => d.state !== 'refused').length ?? 0} drive(s) queued`)
    pane = null
    selected = []
    refresh()
  }

  const bulkFirmware = () => bulk(async () => {
    if (!image) return say('pick a firmware image first')
    const ok = pickedDrives.filter(canFirmware)
    const skip = pickedDrives.length - ok.length
    if (!ok.length) return say('none of the selected drives can take firmware now (busy or failing)')
    if (!confirm(`Update firmware on ${ok.length} drive(s) with ${image}?${skip ? `\n(${skip} skipped: busy or failing)` : ''}\n\nOut-of-fleet drives run in parallel; fleet drives one at a time. Each resets once to activate.`)) return
    const r = await post('api/v1/firmware', { drives: ok.map((d) => d.id), image })
    say(`firmware ${image}: ${r.started.length} started` + (skip ? `, ${skip} skipped` : ''))
    selected = []
  })

  const bulkTest = (kind) => bulk(async () => {
    const ok = pickedDrives.filter(canTest)
    if (!ok.length) return say('none of the selected drives can be tested now')
    summary(await each(ok, 8, (d) => post(`api/v1/drives/${d.id}/test`, { kind })), `${kind.replace('_', ' ')} test`)
  })

  const bulkDesignation = (designation) => bulk(async () => {
    if (designation === 'failed' && !confirm(`Mark ${pickedDrives.length} drive(s) failed? Fleet drives are reported failed to stormblock and drained.`)) return
    summary(await each(pickedDrives, 8, (d) => post(`api/v1/drives/${d.id}/designation`, { designation })), `designation ${designation}`)
  })

  const bulkLocate = (on) => bulk(async () => {
    const ok = pickedDrives.filter(canLocate)
    summary(await each(ok, 8, (d) => post(`api/v1/drives/${d.id}/locate`, { on })), `locate ${on ? 'on' : 'off'}`)
  })
</script>

<div class="page" class:with-pane={!!pane}>
  <header>
    <div class="title">
      <h1>StormDrive</h1>
      {#if node}<span class="sub">{node.node} · v{node.version}</span>{/if}
    </div>
    <div class="counts">
      <span><b>{counts.total}</b> drives</span>
      <span><b>{counts.fleet}</b> fleet</span>
      <span><b>{shelves.length}</b> shelves</span>
      <span><b>{hbas.length}</b> HBAs</span>
      {#if counts.attention}<span class="warn"><b>{counts.attention}</b> attention</span>{/if}
      {#if counts.reformat}<span class="warn"><b>{counts.reformat}</b> need reformat</span>{/if}
      {#if counts.busy}<span class="acc"><b>{counts.busy}</b> busy</span>{/if}
    </div>
    <div class="auth">
      {#if token}
        <span class="ok">signed in</span> <button onclick={signOut}>Sign out</button>
      {:else if signingIn}
        <input type="password" placeholder="bearer (oc whoami -t)" bind:value={pasted} autocomplete="off"
          onkeydown={(e) => e.key === 'Enter' && pasted && signIn()} />
        <button disabled={!pasted} onclick={signIn}>Sign in</button>
        <button onclick={() => (signingIn = false)}>✕</button>
      {:else}
        {#if enforcing}<span class="warn">read only — writes need storage-admin</span>{/if}
        <button onclick={() => (signingIn = true)}>Sign in</button>
      {/if}
    </div>
  </header>

  {#if error}<div class="banner error" role="alert">{error} <button onclick={() => (error = '')}>✕</button></div>{/if}
  {#if notice}<div class="banner">{notice} <button onclick={() => (notice = '')}>✕</button></div>{/if}

  <div class="toolbar">
    <div class="quick">
      {#each QUICK as q}
        <button class:on={quick === q.id} onclick={() => (quick = q.id)}>{q.label}</button>
      {/each}
    </div>
    <input type="search" placeholder="filter: name, serial, model, bay 4, shelf, host…" bind:value={text} />
  </div>

  {#if picked.size}
    <div class="bulk">
      <b>{picked.size} drive{picked.size === 1 ? '' : 's'}</b>
      <fieldset class="plain acts" disabled={!writable} title={writable ? '' : 'needs storage-admin: sign in'}>
      <button disabled={working} onclick={() => bulkTest('smoke')}>Smoke</button>
      <button disabled={working} onclick={() => bulkTest('read_scan')}>Scan</button>
      <span class="sep"></span>
      <button disabled={working} onclick={() => bulkLocate(true)}>💡 Locate</button>
      <button disabled={working} onclick={() => bulkLocate(false)}>◦ Off</button>
      <span class="sep"></span>
      <select disabled={working} onchange={(e) => { if (e.target.value) bulkDesignation(e.target.value); e.target.value = '' }}>
        <option value="">designate…</option>
        <option value="none">none</option>
        <option value="spare">spare</option>
        <option value="reserved">reserved</option>
        <option value="failed">failed</option>
      </select>
      <span class="sep"></span>
      <button disabled={working} onclick={() => bulkPrepare()}>Prepare…</button>
      <button class="danger" disabled={working} onclick={() => bulkPrepare({ format: 4096 })}>Format → 4096…</button>
      <button class="danger" disabled={working} onclick={() => bulkPrepare({ format: 512 })}>→ 512…</button>
      <span class="sep"></span>
      <select bind:value={image} disabled={working}>
        <option value="">firmware image…</option>
        {#each images as i}<option value={i.name}>{i.name}</option>{/each}
      </select>
      <button class="danger" disabled={working || !image} onclick={bulkFirmware}>Update firmware</button>
      </fieldset>
      <button onclick={() => (selected = [])}>Clear</button>
    </div>
  {/if}

  {#if groupRows.length}
    <DataGrid
      columns={groupColumns}
      rows={groupRows}
      getChildren={children}
      selectable="multi"
      bind:selected
      {onaction}
      {onrowclick}
    />
  {:else if drives.length}
    <div class="empty">no drive matches the filter</div>
  {:else}
    <div class="empty">no drives discovered</div>
  {/if}

  <Jobs {jobs} {act} {writable} />
  <fieldset class="plain" disabled={!writable}>
    <Firmware {images} {act} onerror={(e) => (error = e)} />
  </fieldset>
  <Events {events} />
</div>

{#if pane}
  <aside class="pane">
    <button class="close" onclick={() => (pane = null)} title="close">✕</button>
    {#if pane.type === 'prepare'}
      {#key pane.n}
        <Prepare select={pane.select} title={pane.title} preset={pane.preset} {drives} {writable}
          ondone={prepared} onerror={(e) => (error = e)} />
      {/key}
    {:else if paneDrive}
      <fieldset class="plain" disabled={!writable}><DrivePane drive={paneDrive} {images} {act} onprepare={prepare} /></fieldset>
    {:else if paneGroup}
      <fieldset class="plain" disabled={!writable}><GroupPane group={paneGroup} {act} onprepare={prepare} /></fieldset>
    {:else}
      <div class="empty">gone</div>
    {/if}
  </aside>
{/if}

<style>
  :global(body) { margin: 0; background: var(--bg); color: var(--text); font-family: var(--font); font-size: 13px; }
  .page { padding: 16px; }
  .page.with-pane { margin-right: min(460px, 45vw); }
  header { display: flex; align-items: baseline; justify-content: space-between; gap: 16px; flex-wrap: wrap; margin-bottom: 10px; }
  .title { display: flex; align-items: baseline; gap: 10px; }
  h1 { font-size: 17px; margin: 0; }
  .sub { color: var(--text-dim); font-size: 12px; }
  .counts { display: flex; gap: 14px; color: var(--text-dim); font-size: 12px; flex-wrap: wrap; }
  .counts b { color: var(--text); }
  .counts .warn b { color: var(--warn); }
  .counts .acc b { color: var(--accent); }
  .toolbar { display: flex; gap: 10px; align-items: center; flex-wrap: wrap; margin-bottom: 10px; }
  .quick { display: flex; gap: 4px; flex-wrap: wrap; }
  .quick button.on { border-color: var(--accent); color: var(--accent); background: var(--accent-bg); }
  input[type='search'] { flex: 1; min-width: 220px; }
  .bulk { display: flex; gap: 6px; align-items: center; flex-wrap: wrap; padding: 8px 10px; margin-bottom: 10px;
    background: var(--accent-bg); border: 1px solid var(--border); border-radius: var(--radius); position: sticky; top: 0; z-index: 2; }
  .bulk b { margin-right: 6px; }
  .acts { display: flex; gap: 6px; align-items: center; flex-wrap: wrap; }
  .acts[disabled] { opacity: 0.6; }
  fieldset.plain { border: 0; padding: 0; margin: 0; min-width: 0; }
  .auth { display: flex; gap: 6px; align-items: center; font-size: 12px; }
  .auth .ok { color: var(--ok, var(--accent)); }
  .auth .warn { color: var(--warn); }
  .sep { width: 1px; height: 18px; background: var(--border); }
  button.danger { color: var(--error); }
  .banner { display: flex; justify-content: space-between; gap: 10px; align-items: center; padding: 6px 10px; margin-bottom: 10px;
    border: 1px solid var(--border); border-radius: var(--radius-sm); background: var(--panel); font-size: 12px; }
  .banner.error { color: var(--error); border-color: var(--error-border); background: var(--error-bg); }
  .banner button { padding: 0 6px; }
  .empty { color: var(--text-dim); padding: 20px; text-align: center; }
  .pane { position: fixed; top: 0; right: 0; bottom: 0; width: min(460px, 45vw); overflow-y: auto; padding: 16px;
    background: var(--panel); border-left: 1px solid var(--border); box-shadow: var(--shadow); z-index: 3; box-sizing: border-box; }
  .close { position: absolute; top: 10px; right: 10px; padding: 0 8px; }
  @media (max-width: 800px) {
    .page.with-pane { margin-right: 0; }
    .pane { width: 100vw; }
  }
</style>
