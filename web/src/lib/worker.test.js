// npm test — the drive worker's page logic (#38).
import test from 'node:test'
import assert from 'node:assert/strict'
import {
  blankForm, stepsOf, describeStep, destroys, testJob, requestOf, heldDrives, namesDrive, destroyList,
  canCancel, canResume, countsLine, driveLine, sortJobs, openJobOf, prepMetric,
} from './worker.js'

test('the form makes the steps in the order the worker runs them', () => {
  assert.deepEqual(stepsOf(blankForm()), [])
  const f = blankForm({ format: 4096, sanitize: 'crypto', partition: true, enroll: true, tier: ' cold ', role: 'data' })
  assert.deepEqual(stepsOf(f), [
    { op: 'format', block_size: 4096 },
    { op: 'sanitize', method: 'crypto' },
    { op: 'partition', role: 'data' },
    { op: 'enroll', role: 'data', tier: 'cold' },
  ])
  // A select bound to <option value={512}> may hand back a string.
  assert.deepEqual(stepsOf(blankForm({ format: '512' })), [{ op: 'format', block_size: 512 }])
  // Enroll alone, no tier: the server derives it from the drive kind.
  assert.deepEqual(stepsOf(blankForm({ enroll: true, role: 'system' })), [{ op: 'enroll', role: 'system' }])
  assert.equal(stepsOf(f).map(describeStep).join(' → '), 'format → 4096 → sanitize (crypto) → partition (data) → enroll (data slab, tier cold)')
  assert.ok(destroys(stepsOf(f)))
  assert.ok(!destroys(stepsOf(blankForm({ enroll: true }))), 'enroll alone destroys nothing')
  assert.deepEqual(requestOf({ shelf: 'k', unusable: true }, blankForm({ format: 4096 }), ['S1'], true), {
    select: { shelf: 'k', unusable: true }, steps: [{ op: 'format', block_size: 4096 }], destroy: ['S1'], dry_run: true,
  })
})

test('only a drive that holds data can be named for destroy, by its serial', () => {
  const byId = new Map([
    ['a', { id: 'a', serial: 'ZA1' }],
    ['b', { id: 'b', serial: 'ZB2' }],
  ])
  const plan = {
    runnable: 3,
    refused: [
      { drive: 'a', name: 'sda', reason: 'holds a stormblock slab — name it in "destroy" by id, WWN or serial to allow' },
      { drive: 'b', name: 'sdb', reason: 'in the fleet — leave (drain) first' },
    ],
  }
  const held = heldDrives(plan, byId)
  assert.equal(held.length, 1, 'a fleet refusal is not lifted by naming it')
  assert.deepEqual([held[0].name, held[0].serial, held[0].what], ['sda', 'ZA1', 'a stormblock slab'])
  assert.ok(!namesDrive('', held[0]))
  assert.ok(!namesDrive('sda', held[0]), 'a /dev name never counts')
  assert.ok(!namesDrive('za1', held[0]), 'exact serial')
  assert.ok(namesDrive(' ZA1 ', held[0]))
  assert.deepEqual(destroyList(held, { a: 'ZA1' }), ['ZA1'])
  assert.deepEqual(destroyList(held, { a: 'ZA' }), [])
  assert.deepEqual(heldDrives(null, byId), [])
})

test('jobs: what can be cancelled or resumed, and how a drive reads', () => {
  const steps = [{ op: 'format', block_size: 4096 }, { op: 'partition', role: 'data' }]
  const job = (states, extra = {}) => ({
    id: 'j', steps, finished: states.every((s) => !['queued', 'running', 'interrupted'].includes(s)),
    drives: states.map((state, i) => ({ drive: `d${i}`, name: `sd${i}`, state, step: 0, phase: state })),
    counts: Object.fromEntries(states.map((s) => [s, states.filter((x) => x === s).length])),
    ...extra,
  })
  assert.ok(canCancel(job(['queued', 'running'])))
  assert.ok(!canCancel(job(['running', 'done'])), 'a running step is never stopped')
  assert.ok(canResume(job(['interrupted'])) && canCancel(job(['interrupted'])))
  assert.ok(!canResume(job(['done', 'failed'])))
  assert.equal(countsLine(job(['done', 'running', 'done', 'refused'])), '1 running · 2 done · 1 refused')
  assert.equal(driveLine({ state: 'running', step: 0, phase: 'formatting', progress_pct: 42 }, steps), 'running · format → 4096 42% · formatting')
  assert.equal(driveLine({ state: 'refused', step: 0, phase: 'refused' }, steps), 'refused')

  const old = job(['done'], { id: 'old', created: { secs_since_epoch: 2 } })
  const open = job(['running'], { id: 'open', created: { secs_since_epoch: 1 } })
  const newer = job(['done'], { id: 'newer', created: { secs_since_epoch: 3 } })
  assert.deepEqual(sortJobs([old, newer, open]).map((j) => j.id), ['open', 'newer', 'old'])
  assert.equal(openJobOf([old, open], 'd0')?.id, 'open')
  assert.equal(openJobOf([old], 'd0'), undefined)
})

test('the prep phase shows when it adds something', () => {
  assert.deepEqual(prepMetric({ membership: 'out', prep: { phase: 'unusable' } }), { label: 'prep', value: 'unusable', tone: 'warn' })
  assert.equal(prepMetric({ membership: 'out', prep: { phase: 'ready' } }).value, 'ready')
  assert.equal(prepMetric({ membership: 'fleet', prep: { phase: 'enrolled' } }), null)
  assert.equal(prepMetric({ membership: 'out', prep: { phase: 'formatting', pct: 3 } }), null, 'the activity metric shows progress')
  assert.equal(prepMetric({ membership: 'out' }), null)
})

test('ATA security erase (#36): its own step, enhanced when asked', () => {
  assert.deepEqual(stepsOf(blankForm({ sanitize: 'ata', partition: true })), [{ op: 'security_erase' }, { op: 'partition', role: 'data' }])
  const s = stepsOf(blankForm({ sanitize: 'ata-enhanced' }))
  assert.deepEqual(s, [{ op: 'security_erase', enhanced: true }])
  assert.equal(describeStep(s[0]), 'security erase (enhanced)')
  assert.equal(describeStep({ op: 'security_erase' }), 'security erase')
  assert.equal(destroys(s), true)
})

test('a test step (#40): read-only unless destructive, and the bulk test is one job', () => {
  const s = stepsOf(blankForm({ format: 4096, test: 'smoke', partition: true, enroll: true }))
  assert.deepEqual(s.map((x) => x.op), ['format', 'test', 'partition', 'enroll'])
  assert.equal(describeStep(s[1]), 'test (smoke)')
  assert.equal(destroys([{ op: 'test', kind: 'smoke' }]), false)
  assert.equal(destroys([{ op: 'test', kind: 'read_scan' }, { op: 'enroll', role: 'data' }]), false)
  assert.equal(destroys([{ op: 'test', kind: 'destructive_sample' }]), true)
  assert.deepEqual(testJob(['a', 'b'], 'read_scan'), { select: { drives: ['a', 'b'] }, steps: [{ op: 'test', kind: 'read_scan' }], destroy: [], dry_run: false })
})
