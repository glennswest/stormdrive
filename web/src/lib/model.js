// Pure page logic: grouping, filtering, what a drive may do. No DOM, no
// Svelte, so `node --test src/lib/` covers it (model.test.js).
//
// The eligibility rules mirror the server's guards in src/drive.rs
// (fleet_join_blocker, destructive_test_blocker, format_blocker,
// firmware_blocker). The server has the last word: these only decide which
// buttons are offered.

export function human(b) {
  if (!b) return '0'
  const u = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB']
  let i = 0
  while (b >= 1024 && i < u.length - 1) {
    b /= 1024
    i++
  }
  return b.toFixed(b >= 100 || i === 0 ? 0 : 1) + ' ' + u[i]
}

/// Shelf::key() in src/drive.rs: logical id, else serial, else sysfs id.
export function shelfKey(shelf) {
  if (!shelf) return null
  return shelf.logical_id || shelf.serial || shelf.id || null
}

export function shortKey(k) {
  return k && k.length > 10 ? '…' + k.slice(-6) : k || ''
}

// ---------------------------------------------------------------- health

/// A drive's health as a HealthDot value.
export function healthDot(d) {
  if (d.activity === 'missing') return 'error'
  const s = d.health?.status || 'unknown'
  return { good: 'ok', warning: 'warn', failing: 'error', failed: 'error' }[s] || 'unknown'
}

/// An SES element/overall status as a HealthDot value.
export function sesDot(s) {
  return (
    { ok: 'ok', noncritical: 'warn', critical: 'error', unrecoverable: 'error', info: 'ok' }[s] ||
    'unknown'
  )
}

// ------------------------------------------------------------ eligibility

const BUSY = ['testing', 'formatting', 'sanitizing', 'updating_firmware', 'draining']

export const isIdle = (d) => d.activity === 'idle'
export const canJoin = (d) =>
  d.membership !== 'fleet' &&
  isIdle(d) &&
  d.usable &&
  !d.in_use_by &&
  d.designation !== 'failed' &&
  d.designation !== 'reserved' &&
  !['failing', 'failed'].includes(d.health?.status)
export const canLeave = (d) => d.membership === 'fleet'
export const canTest = (d) => isIdle(d) && d.usable
export const canDestructive = (d) => canTest(d) && d.membership !== 'fleet' && !d.in_use_by
export const canFormat = (d) =>
  d.membership !== 'fleet' &&
  !d.in_use_by &&
  isIdle(d) &&
  d.designation !== 'reserved' &&
  d.kind !== 'nvme_ssd'
export const canFirmware = (d) =>
  isIdle(d) && !['failing', 'failed'].includes(d.health?.status)
export const canForget = (d) => d.activity === 'missing' && d.membership !== 'fleet'
export const canLocate = (d) =>
  d.location?.bay != null || !!d.location?.pcie_slot

/// The sector size a single "Format" button goes to.
export function formatTarget(d) {
  if (d.needs_reformat) return 4096
  return d.block_size === 4096 ? 512 : 4096
}

// ---------------------------------------------------------------- filters

export const QUICK = [
  { id: 'all', label: 'All' },
  { id: 'attention', label: 'Attention' },
  { id: 'reformat', label: 'Needs reformat' },
  { id: 'out', label: 'Out of fleet' },
  { id: 'fleet', label: 'Fleet' },
  { id: 'busy', label: 'Busy' },
]

export function needsAttention(d) {
  return (
    ['warning', 'failing', 'failed'].includes(d.health?.status) ||
    d.designation === 'failed' ||
    d.activity === 'missing' ||
    d.activity === 'draining' ||
    !!d.needs_reformat
  )
}

export function matchesQuick(d, quick) {
  switch (quick) {
    case 'attention':
      return needsAttention(d)
    case 'reformat':
      return !!d.needs_reformat
    case 'out':
      return d.membership !== 'fleet'
    case 'fleet':
      return d.membership === 'fleet'
    case 'busy':
      return BUSY.includes(d.activity)
    default:
      return true
  }
}

/// Free text: every word must appear in the drive's name, model, serial,
/// WWID, firmware, kind, bay ("bay 4"), slot, shelf or HBA.
export function matchesText(d, text) {
  const words = (text || '').toLowerCase().split(/\s+/).filter(Boolean)
  if (!words.length) return true
  const l = d.location || {}
  const hay = [
    d.name,
    d.path,
    d.model,
    d.serial,
    d.wwid,
    d.firmware,
    d.kind,
    d.designation,
    d.membership,
    l.bay != null ? `bay ${l.bay}` : '',
    l.pcie_slot ? `slot ${l.pcie_slot}` : '',
    l.pcie_addr,
    l.shelf?.model,
    shelfKey(l.shelf),
    l.controller?.scsi_host,
    l.controller?.pcie_addr,
  ]
    .filter(Boolean)
    .join(' ')
    .toLowerCase()
  return words.every((w) => hay.includes(w))
}

// --------------------------------------------------------------- grouping

function byBayThenName(a, b) {
  const x = a.location?.bay ?? Infinity
  const y = b.location?.bay ?? Infinity
  if (x !== y) return x - y
  return String(a.name).localeCompare(String(b.name), undefined, { numeric: true })
}

/// Where a drive is grouped: its shelf, else its HBA (direct-attached SAS/
/// SATA), else NVMe on PCIe, else unlocated.
export function groupIdOf(d) {
  const l = d.location || {}
  const sk = shelfKey(l.shelf)
  if (sk) return `shelf:${sk}`
  if (l.controller?.pcie_addr) return `hba:${l.controller.pcie_addr}`
  if (d.kind === 'nvme_ssd' || l.pcie_addr || l.pcie_slot) return 'nvme'
  return 'unlocated'
}

/// The page's top rows: one group per shelf, per HBA with direct drives,
/// NVMe, and unlocated — each with the drives that pass the filter.
///
/// `shelves` is /api/v1/shelves (SES reports, so an empty shelf still
/// shows), `hbas` /api/v1/hbas. With a filter set, a group with no matching
/// drive is dropped; without one, every shelf and every HBA that nothing
/// else accounts for is kept, so new hardware shows before its drives do.
export function groupDrives(drives, shelves = [], hbas = [], filter = {}) {
  const filtering = (filter.quick && filter.quick !== 'all') || (filter.text || '').trim()
  const groups = new Map()
  const add = (id, make) => {
    if (!groups.has(id)) groups.set(id, { id, drives: [], all: 0, ...make() })
    return groups.get(id)
  }
  const reports = new Map(shelves.map((s) => [s.key, s]))
  const hbaByAddr = new Map(hbas.map((h) => [h.pcie_addr, h]))
  const behindHba = new Set()

  for (const d of drives) {
    const gid = groupIdOf(d)
    const l = d.location || {}
    if (l.controller?.pcie_addr) behindHba.add(l.controller.pcie_addr)
    const g = add(gid, () => {
      if (gid.startsWith('shelf:')) {
        const key = gid.slice(6)
        return { kind: 'shelf', key, shelf: l.shelf, report: reports.get(key) || null }
      }
      if (gid.startsWith('hba:')) {
        const addr = gid.slice(4)
        return { kind: 'hba', key: addr, hba: hbaByAddr.get(addr) || null, controller: l.controller }
      }
      return { kind: gid, key: gid }
    })
    g.all++
    if (matchesQuick(d, filter.quick) && matchesText(d, filter.text)) g.drives.push(d)
  }
  if (!filtering) {
    for (const r of shelves) add(`shelf:${r.key}`, () => ({ kind: 'shelf', key: r.key, shelf: r.shelf, report: r }))
    for (const h of hbas)
      if (!behindHba.has(h.pcie_addr)) add(`hba:${h.pcie_addr}`, () => ({ kind: 'hba', key: h.pcie_addr, hba: h }))
  }

  const rank = { shelf: 0, hba: 1, nvme: 2, unlocated: 3 }
  const out = [...groups.values()]
    .filter((g) => !filtering || g.drives.length)
    .map((g) => ({ ...g, drives: g.drives.sort(byBayThenName), label: groupLabel(g) }))
  out.sort((a, b) => rank[a.kind] - rank[b.kind] || a.label.localeCompare(b.label, undefined, { numeric: true }))
  return out
}

export function groupLabel(g) {
  switch (g.kind) {
    case 'shelf': {
      const s = g.report?.shelf || g.shelf || {}
      return [s.vendor, s.model || 'shelf', shortKey(g.key)].filter(Boolean).join(' ')
    }
    case 'hba': {
      const name = g.hba?.board_name || g.hba?.driver || g.controller?.driver || 'HBA'
      return `${name} ${g.key}`
    }
    case 'nvme':
      return 'NVMe (PCIe)'
    default:
      return 'Unlocated'
  }
}

/// Ticked rows → drive ids. A ticked group means every drive it shows
/// (under the current filter); a ticked drive means itself.
export function selectedDrives(selected, groups) {
  const ids = new Set()
  const sel = new Set(selected)
  for (const g of groups) {
    const whole = sel.has(g.id)
    for (const d of g.drives) if (whole || sel.has(d.id)) ids.add(d.id)
  }
  return ids
}

// The volumes with legs on a drive (#26), one line each: what it is, who
// uses it, what of it is here, and how it stands. `trouble` = a leg here is
// draining, quarantined, failed or missing, or the volume owes a rebuild.
export function volumeLine(v) {
  const c = v.consumer
  const who = c ? `${c.kind} ${c.namespace ? c.namespace + '/' : ''}${c.name}` : 'unclaimed'
  const shared = v.shared_legs ? `, ${v.shared_legs} shared` : ''
  const bits = [`${human(v.bytes)} · ${v.legs} leg${v.legs === 1 ? '' : 's'}${shared}`]
  if (v.policy) bits.push(v.health && v.health !== 'healthy' ? `${v.policy} ${v.health}` : v.policy)
  if (v.state && v.state !== 'ok') bits.push(`here: ${v.state}`)
  if (v.rebuild && v.rebuild !== 'none') bits.push(`rebuild ${v.rebuild}`)
  return {
    name: v.name || v.id,
    kind: v.kind === 'volume' ? '' : v.kind,
    who,
    detail: bits.join(' · '),
    trouble: (v.state && v.state !== 'ok') || (v.rebuild && v.rebuild !== 'none'),
  }
}

// Days to wear-out by the trend (#23): "in 412 days (2027-11-23)", or
// "now" once the line has reached 100 %.
export function wearOut(p) {
  if (!p) return ''
  if (!p.days_left) return 'now (rated endurance reached)'
  const when = new Date(p.wear_out_unix * 1000).toISOString().slice(0, 10)
  return `in ${p.days_left} days (${when}), ${p.rate_pct_per_day.toFixed(3)} %/day`
}
