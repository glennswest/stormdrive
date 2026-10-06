// Pure logic for the drive worker on the page (#38): the request a Prepare
// form makes, which refusals a `destroy` naming would lift, and what a job
// says. No DOM, no Svelte (worker.test.js). The server (src/worker.rs) has
// the last word: it validates the steps and re-checks every guard.

export const SANITIZE = ['block', 'crypto', 'overwrite']

/// A fresh Prepare form; `preset` overrides (e.g. a format-only job).
export function blankForm(preset = {}) {
  return { format: 0, sanitize: '', partition: false, enroll: false, tier: '', role: 'data', ...preset }
}

/// The steps, in the order the worker runs them: low-level (format, then
/// sanitize), partition, enroll. A partition and its slab share the role.
export function stepsOf(f) {
  const s = []
  if (f.format) s.push({ op: 'format', block_size: Number(f.format) })
  if (f.sanitize) s.push({ op: 'sanitize', method: f.sanitize })
  if (f.partition) s.push({ op: 'partition', role: f.role })
  if (f.enroll) {
    const e = { op: 'enroll', role: f.role }
    if ((f.tier || '').trim()) e.tier = f.tier.trim()
    s.push(e)
  }
  return s
}

export function describeStep(s) {
  switch (s.op) {
    case 'format':
      return `format → ${s.block_size}`
    case 'sanitize':
      return `sanitize (${s.method})`
    case 'partition':
      return `partition (${s.role})`
    case 'enroll':
      return `enroll (${s.role} slab${s.tier ? `, tier ${s.tier}` : ''})`
    default:
      return s.op
  }
}

/// Steps that destroy what is on the drive (format, sanitize, partition).
export const destroys = (steps) => steps.some((s) => s.op !== 'enroll')

/// `select` is a worker Select: {drives: [ids]} or {shelf, bays?, unusable?}.
export function requestOf(select, form, destroy = [], dryRun = false) {
  return { select, steps: stepsOf(form), destroy, dry_run: dryRun }
}

/// Refusals a `destroy` naming lifts: the drive holds a slab or a
/// filesystem (worker::guard's "holds … — name it in "destroy""). Every
/// other refusal (fleet, busy, reserved, mounted, …) stays refused.
export function heldDrives(plan, byId) {
  return (plan?.refused || [])
    .filter((r) => /^holds /.test(r.reason || ''))
    .map((r) => ({ ...r, serial: byId.get(r.drive)?.serial || '', what: r.reason.replace(/^holds (.*?) — .*$/, '$1') }))
}

/// What the operator typed names the drive: its serial exactly (the
/// server also takes the stable id or WWN; the page asks for the serial
/// printed on the label). A /dev name never counts.
export function namesDrive(typed, held) {
  const t = (typed || '').trim()
  return !!t && !!held.serial && t === held.serial
}

/// The destroy list from what was typed: one serial per held drive whose
/// confirmation matches.
export function destroyList(held, typed) {
  return held.filter((h) => namesDrive(typed[h.drive], h)).map((h) => h.serial)
}

// ------------------------------------------------------------------ jobs

const OPEN = ['queued', 'running', 'interrupted']

export function jobOpen(j) {
  return !j.finished
}

/// Cancel stops what has not started: queued or interrupted drives.
export const canCancel = (j) => (j.drives || []).some((d) => d.state === 'queued' || d.state === 'interrupted')
export const canResume = (j) => (j.drives || []).some((d) => d.state === 'interrupted')

/// "3 running · 1 done · 2 refused" in a stable order.
export function countsLine(j) {
  const order = ['running', 'queued', 'interrupted', 'done', 'failed', 'refused', 'cancelled']
  const c = j.counts || {}
  return order.filter((k) => c[k]).map((k) => `${c[k]} ${k}`).join(' · ')
}

/// One drive's line in a job: where it is and how far.
export function driveLine(dj, steps) {
  const step = steps?.[dj.step]
  const at = dj.state === 'running' && step ? describeStep(step) : ''
  const pct = dj.progress_pct != null ? ` ${dj.progress_pct}%` : ''
  return [dj.state, at && `${at}${pct}`, dj.phase && dj.phase !== dj.state ? dj.phase : ''].filter(Boolean).join(' · ')
}

/// Open jobs first, newest first within each.
export function sortJobs(jobs) {
  const t = (j) => j.created?.secs_since_epoch || 0
  return [...jobs].sort((a, b) => Number(jobOpen(b)) - Number(jobOpen(a)) || t(b) - t(a))
}

/// Is this drive in an open job? (its row says so)
export function openJobOf(jobs, driveId) {
  return jobs.find((j) => jobOpen(j) && (j.drives || []).some((d) => d.drive === driveId && OPEN.includes(d.state)))
}

// ------------------------------------------------------------- prep phase

/// The drive's `prep` phase as a grid metric, when it says something the
/// rest of the state column does not.
export function prepMetric(d) {
  const p = d.prep
  if (!p || d.membership === 'fleet') return null
  if (p.phase === 'unusable') return { label: 'prep', value: 'unusable', tone: 'warn' }
  if (p.phase === 'ready') return { label: 'prep', value: 'ready', tone: 'ok' }
  return null
}
