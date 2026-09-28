// The built page (dist/assets/app.js) in jsdom against a mocked API with a
// node's worth of hardware: two 24-bay NetApp shelves, an HBA with four
// direct drives, 160 NVMe bays (#6). Run after `npm run build`:
//   npm run test:page        (web/rebuild.sh runs it on dev)
import test from 'node:test'
import assert from 'node:assert/strict'
import { JSDOM } from 'jsdom'
import { fileURLToPath } from 'node:url'

const ctrl = (n) => ({ scsi_host: `host${n}`, pcie_addr: `0000:0${n + 1}:00.0`, driver: 'mpt3sas' })
const SHELVES = ['5000a098aaaa0001', '5000a098bbbb0002']

function drive(i, over) {
  return {
    id: `d-${i}`, name: `sd${i}`, path: `/dev/sd${i}`, paths: [`/dev/sd${i}`],
    kind: 'sas_hdd', model: 'ST1200MM0098', serial: `S${i}`, firmware: 'N003', wwid: `naa.5000c500${i}`,
    capacity_bytes: 1.2e12, block_size: 512, physical_block_size: 512, usable: true,
    membership: 'out', designation: 'none', activity: 'idle', overcommit: { enabled: false, ratio: 1 },
    health: { status: 'good', temperature_c: 34, media_errors: 0, messages: [] },
    location: {}, needs_reformat: false, usage: null, ...over,
  }
}

function fixture() {
  const drives = []
  SHELVES.forEach((key, s) => {
    for (let bay = 0; bay < 24; bay++) {
      const i = drives.length
      drives.push(drive(i, {
        location: { shelf: { logical_id: key, model: 'DS224C', vendor: 'NETAPP' }, bay, controller: ctrl(0) },
        // The first shelf arrived from NetApp at 520-byte sectors.
        ...(s === 0 ? { block_size: 520, usable: false, needs_reformat: true } : { membership: 'fleet' }),
      }))
    }
  })
  for (let bay = 0; bay < 4; bay++) drives.push(drive(drives.length, { kind: 'sata_hdd', location: { bay, controller: ctrl(1) } }))
  for (let slot = 1; slot <= 160; slot++) {
    const i = drives.length
    drives.push(drive(i, {
      name: `nvme${slot}n1`, kind: 'nvme_ssd', model: 'Micron 7450',
      location: { pcie_addr: `10000:${slot.toString(16)}:00.0`, pcie_slot: String(slot), bay: slot },
      health: { status: slot === 7 ? 'warning' : 'good', temperature_c: 40, wear_pct: 3, media_errors: 0, messages: [] },
    }))
  }
  const report = (key) => ({
    key, shelf: { logical_id: key, model: 'DS224C', vendor: 'NETAPP' }, display: `DS224C ${key}`,
    esps: [{ scsi_id: '0:0:24:0' }, { scsi_id: '1:0:24:0' }], paths: 2, status: 'ok',
    power_supplies: { ok: 2, total: 2 }, fans: { ok: 4, total: 4 }, slots: { ok: 24, total: 24 },
    max_temperature_c: 29, elements: [], drives: [],
  })
  return {
    'api/v1/health': { status: 'ok', version: '0.16.0', node: 'test-node' },
    'api/v1/drives': { drives },
    'api/v1/shelves': { shelves: SHELVES.map(report) },
    'api/v1/events': { latest_seq: 1, events: [{ seq: 1, time: { secs_since_epoch: 1790000000 }, severity: 'info', kind: 'discovered', message: 'sd0: discovered' }] },
    'api/v1/firmware/images': { images: [] },
    'api/v1/hbas': { hbas: [{ pcie_addr: '0000:01:00.0', driver: 'mpt3sas', firmware: '16.00.10.00' }, { pcie_addr: '0000:02:00.0', driver: 'mpt3sas' }] },
  }
}

const tick = (ms = 0) => new Promise((r) => setTimeout(r, ms))

test('the built page renders a 212-drive node grouped, and stays put across a refresh', async () => {
  const dom = new JSDOM('<!doctype html><html><body><div id="app"></div></body></html>', {
    url: 'http://localhost:9092/', pretendToBeVisual: true,
  })
  const w = dom.window
  for (const k of Object.getOwnPropertyNames(w)) {
    if (!(k in globalThis)) {
      try { globalThis[k] = w[k] } catch {}
    }
  }
  globalThis.window = w
  globalThis.document = w.document
  // The page's 4 s poll runs on jsdom's timers, so w.close() stops it and
  // node can exit.
  globalThis.setInterval = w.setInterval.bind(w)
  globalThis.clearInterval = w.clearInterval.bind(w)
  const data = fixture()
  const calls = []
  globalThis.fetch = async (url, opts = {}) => {
    const path = String(url).replace(/^\//, '').split('?')[0]
    calls.push(`${opts.method || 'GET'} ${path}`)
    const body = data[path] ?? {}
    return { ok: true, status: 200, statusText: 'OK', json: async () => structuredClone(body) }
  }

  const app = fileURLToPath(new URL('../dist/assets/app.js', import.meta.url))
  await import(app)
  await tick(100)
  const doc = w.document
  const text = () => doc.body.textContent

  assert.match(text(), /StormDrive/)
  assert.match(text(), /test-node · v0\.16\.0/)
  assert.match(text(), /212\s*drives/)
  assert.match(text(), /24\s*need reformat/)

  // Top rows are groups: two shelves, the HBA with direct drives, NVMe.
  // The card behind the shelves is not a group of its own.
  const top = () => [...doc.querySelectorAll('#app > .page > .grid-wrap > table > tbody > tr:not(.child-row)')]
  assert.deepEqual(top().map((r) => r.textContent.match(/NETAPP DS224C \S+|mpt3sas \S+|NVMe \(PCIe\)/)?.[0]),
    ['NETAPP DS224C …aa0001', 'NETAPP DS224C …bb0002', 'mpt3sas 0000:02:00.0', 'NVMe (PCIe)'])
  assert.equal(doc.querySelectorAll('.nested').length, 0, 'collapsed: no drive rows rendered')

  // Expand NVMe: 160 rows in a nested grid.
  const nvme = top()[3]
  const t0 = performance.now()
  nvme.querySelector('button.expander').click()
  await tick()
  const expandMs = performance.now() - t0
  const nested = () => [...doc.querySelectorAll('.nested tbody tr')]
  assert.equal(nested().length, 160)
  console.log(`# expanding 160 NVMe rows: ${expandMs.toFixed(0)} ms in jsdom`)
  const firstRow = nested()[0]
  assert.match(firstRow.textContent, /nvme1n1/)

  // Tick the first shelf's group row: the bulk bar counts its 24 drives.
  top()[0].querySelector('td.ctl input[type=checkbox]').click()
  await tick()
  assert.match(doc.querySelector('.bulk').textContent, /24 drives/)

  // Click a drive row: the pane opens on it.
  firstRow.click()
  await tick()
  assert.match(doc.querySelector('aside.pane').textContent, /nvme1n1[\s\S]*Micron 7450[\s\S]*PCIe slot 1/)

  // The 4 s refresh diffs keyed rows: the same row element survives.
  const before = calls.filter((c) => c.endsWith('api/v1/drives')).length
  await tick(4300)
  assert.ok(calls.filter((c) => c.endsWith('api/v1/drives')).length > before, 'refreshed')
  assert.equal(nested()[0], firstRow, 'row element kept across a refresh')

  // Quick filter: "Needs reformat" leaves only the 520-byte shelf.
  const btn = [...doc.querySelectorAll('.quick button')].find((b) => b.textContent === 'Needs reformat')
  btn.click()
  await tick()
  assert.equal(top().length, 1)
  assert.match(top()[0].textContent, /…aa0001/)
  w.close()
})
