// node --test src/lib/  (run on dev: sc-build 'cd web && npm ci && npm test')
import test from 'node:test'
import assert from 'node:assert/strict'
import {
  groupDrives,
  groupIdOf,
  selectedDrives,
  matchesText,
  canJoin,
  canFormat,
  canDestructive,
  formatTarget,
  healthDot,
  human,
} from './model.js'

const drive = (over = {}) => ({
  id: over.name || 'x',
  name: 'sda',
  kind: 'sas_hdd',
  model: 'ST1200MM0098',
  serial: 'S1',
  membership: 'out',
  designation: 'none',
  activity: 'idle',
  usable: true,
  block_size: 512,
  health: { status: 'good' },
  location: {},
  ...over,
})

const shelf = { logical_id: '5000a098aaaa0001', model: 'DS224C' }
const ctrl = { scsi_host: 'host0', pcie_addr: '0000:01:00.0', driver: 'mpt3sas' }

test('drives group by shelf, then HBA, then NVMe, then unlocated', () => {
  const ds = [
    drive({ name: 'sdc', location: { shelf, bay: 9, controller: ctrl } }),
    drive({ name: 'sdb', location: { shelf, bay: 2, controller: ctrl } }),
    drive({ name: 'sda', location: { controller: ctrl, bay: 0 } }),
    drive({ name: 'nvme0n1', kind: 'nvme_ssd', location: { pcie_addr: '0000:5e:00.0', pcie_slot: '3' } }),
    drive({ name: 'vda', kind: 'unknown' }),
  ]
  const g = groupDrives(ds, [], [])
  assert.deepEqual(g.map((x) => x.kind), ['shelf', 'hba', 'nvme', 'unlocated'])
  assert.deepEqual(g[0].drives.map((d) => d.name), ['sdb', 'sdc'], 'ordered by bay')
  assert.equal(g[0].label, 'DS224C …aa0001')
  assert.equal(groupIdOf(ds[0]), 'shelf:5000a098aaaa0001')
})

test('an empty SES shelf and a card with no drives still show, but not under a filter', () => {
  const shelves = [{ key: 'k2', shelf: { model: 'DS212C', logical_id: 'k2' } }]
  const hbas = [{ pcie_addr: '0000:02:00.0', driver: 'mpt3sas' }, { pcie_addr: '0000:01:00.0' }]
  const ds = [drive({ location: { shelf, controller: ctrl } })]
  const all = groupDrives(ds, shelves, hbas)
  assert.deepEqual(all.map((g) => g.id), ['shelf:k2', `shelf:${shelf.logical_id}`, 'hba:0000:02:00.0'],
    'the HBA with a shelf behind it is not a separate empty group')
  const filtered = groupDrives(ds, shelves, hbas, { quick: 'reformat' })
  assert.equal(filtered.length, 0)
})

test('a ticked group is every drive it shows; a ticked drive is itself', () => {
  const ds = [
    drive({ name: 'sdb', needs_reformat: true, location: { shelf } }),
    drive({ name: 'sdc', location: { shelf } }),
    drive({ name: 'sdd', location: { controller: ctrl } }),
  ]
  const g = groupDrives(ds, [], [], { quick: 'reformat' })
  assert.deepEqual([...selectedDrives([g[0].id], g)], ['sdb'], 'filter narrows the group')
  const all = groupDrives(ds)
  assert.deepEqual([...selectedDrives(['sdd'], all)], ['sdd'])
})

test('text filter: every word, across location and identity', () => {
  const d = drive({ name: 'sdq', serial: 'WX11', location: { shelf, bay: 14, controller: ctrl } })
  assert.ok(matchesText(d, 'bay 14'))
  assert.ok(matchesText(d, 'ds224c wx11'))
  assert.ok(!matchesText(d, 'bay 15'))
  assert.ok(matchesText(d, ''))
})

test('eligibility mirrors the server guards', () => {
  assert.ok(canJoin(drive()))
  assert.ok(!canJoin(drive({ in_use_by: 'stormblock' })))
  assert.ok(!canJoin(drive({ designation: 'reserved' })))
  assert.ok(!canJoin(drive({ usable: false })))
  assert.ok(canFormat(drive({ usable: false, needs_reformat: true })))
  assert.ok(!canFormat(drive({ kind: 'nvme_ssd' })))
  assert.ok(!canFormat(drive({ membership: 'fleet' })))
  assert.ok(!canDestructive(drive({ membership: 'fleet' })))
  assert.equal(formatTarget(drive({ block_size: 520, needs_reformat: true })), 4096)
  assert.equal(formatTarget(drive({ block_size: 4096 })), 512)
  assert.equal(healthDot(drive({ activity: 'missing' })), 'error')
  assert.equal(healthDot(drive({ health: { status: 'warning' } })), 'warn')
  assert.equal(human(1536), '1.5 KiB')
})
