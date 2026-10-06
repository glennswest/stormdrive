// The built page's drive-worker flow (#38) and sign-in (#47) in jsdom,
// against a mocked API on a node that enforces the write gate:
//   read only until a bearer is pasted → tick a 520-byte shelf → Format →
//   4096… → dry run (22 run, one holds a slab, one busy) → type the held
//   drive's serial → run (bearer + destroy sent) → the job in the Jobs panel.
// Run after `npm run build`:  npm run test:page
import test from 'node:test'
import assert from 'node:assert/strict'
import { JSDOM } from 'jsdom'
import { fileURLToPath } from 'node:url'

const KEY = '5000a098aaaa0001'
const ctrl = { scsi_host: 'host0', pcie_addr: '0000:01:00.0', driver: 'mpt3sas' }

function drive(bay) {
  return {
    id: `d-${bay}`, name: `sd${bay}`, path: `/dev/sd${bay}`, paths: [`/dev/sd${bay}`],
    kind: 'sas_hdd', model: 'ST1200MM0098', serial: `S${bay}`, firmware: 'N003', wwid: `naa.5000c500${bay}`,
    capacity_bytes: 1.2e12, block_size: 520, physical_block_size: 520, usable: false, needs_reformat: true,
    membership: 'out', designation: 'none', activity: bay === 1 ? 'testing' : 'idle', overcommit: { enabled: false, ratio: 1 },
    health: { status: 'good', temperature_c: 30, media_errors: 0, messages: [] },
    location: { shelf: { logical_id: KEY, model: 'DS224C', vendor: 'NETAPP' }, bay, controller: ctrl },
    prep: { phase: bay === 1 ? 'busy' : 'unusable', pct: null }, usage: null,
  }
}

const tick = (ms = 0) => new Promise((r) => setTimeout(r, ms))

test('sign in, prepare a shelf through a dry run and a destroy confirmation, follow the job', async () => {
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
  globalThis.sessionStorage = w.sessionStorage
  globalThis.setInterval = w.setInterval.bind(w)
  globalThis.clearInterval = w.clearInterval.bind(w)
  const confirms = []
  w.confirm = globalThis.confirm = (m) => (confirms.push(m), true)

  const drives = Array.from({ length: 24 }, (_, b) => drive(b))
  let jobs = []
  const posts = []
  const data = {
    'api/v1/health': { status: 'ok', version: '0.20.0', node: 'test-node', writes: { gate: 'enforce', apiserver: true, admin_token: false } },
    'api/v1/drives': { drives },
    'api/v1/shelves': { shelves: [{ key: KEY, shelf: { logical_id: KEY, model: 'DS224C', vendor: 'NETAPP' }, esps: [], paths: 1, status: 'ok',
      power_supplies: { ok: 2, total: 2 }, fans: { ok: 4, total: 4 }, slots: { ok: 24, total: 24 }, elements: [], drives: [] }] },
    'api/v1/events': { latest_seq: 0, events: [] },
    'api/v1/firmware/images': { images: [] },
    'api/v1/hbas': { hbas: [] },
  }
  const plan = {
    dry_run: true, steps: ['format → 4096'], runnable: 22,
    drives: drives.filter((d) => d.location.bay > 1).map((d) => ({ drive: d.id, name: d.name })),
    refused: [
      { drive: 'd-0', name: 'sd0', reason: 'holds a stormblock slab — name it in "destroy" by id, WWN or serial to allow' },
      { drive: 'd-1', name: 'sd1', reason: 'busy: testing' },
    ],
  }
  globalThis.fetch = async (url, opts = {}) => {
    const path = String(url).replace(/^\//, '').split('?')[0]
    const method = opts.method || 'GET'
    let body
    if (method === 'GET') body = path === 'api/v1/worker/jobs' ? { jobs } : data[path] ?? {}
    else {
      const req = JSON.parse(opts.body || '{}')
      posts.push({ path, req, auth: opts.headers?.Authorization })
      if (path === 'api/v1/worker/jobs' && req.dry_run) body = plan
      else if (path === 'api/v1/worker/jobs') {
        if (!opts.headers?.Authorization) return { ok: false, status: 401, statusText: 'Unauthorized', json: async () => ({ error: 'a bearer is required', code: 'unauthorized' }) }
        const job = {
          id: 'j1', created: { secs_since_epoch: 1790000000 }, steps: req.steps, finished: false,
          requester: { who: 'kubernetes:alice' }, counts: { running: 1, queued: 22, refused: 1 },
          drives: [
            { drive: 'd-0', name: 'sd0', serial: 'S0', state: 'running', step: 0, phase: 'formatting', progress_pct: 10 },
            ...drives.slice(2).map((d) => ({ drive: d.id, name: d.name, serial: d.serial, state: 'queued', step: 0, phase: 'queued' })),
            { drive: 'd-1', name: 'sd1', serial: 'S1', state: 'refused', step: 0, phase: 'refused', error: 'busy: testing' },
          ],
        }
        jobs = [job]
        body = job
      } else body = {}
    }
    return { ok: true, status: 200, statusText: 'OK', json: async () => structuredClone(body) }
  }

  const app = fileURLToPath(new URL('../dist/assets/app.js', import.meta.url))
  await import(app)
  await tick(100)
  const doc = w.document
  const text = () => doc.body.textContent
  const button = (re, root = doc) => [...root.querySelectorAll('button')].find((b) => re.test(b.textContent))
  const type = async (el, v) => {
    el.value = v
    el.dispatchEvent(new w.Event('input', { bubbles: true }))
    await tick()
  }

  // Enforcing, no bearer: read only, and the bulk bar's actions are off.
  assert.match(text(), /read only — writes need storage-admin/)
  const top = () => [...doc.querySelectorAll('#app > .page > .grid-wrap > table > tbody > tr:not(.child-row)')]
  top()[0].querySelector('td.ctl input[type=checkbox]').click()
  await tick()
  assert.match(doc.querySelector('.bulk').textContent, /24 drives/)
  assert.ok(doc.querySelector('.bulk fieldset').disabled, 'bulk actions disabled while read only')

  // Sign in: the bearer goes to sessionStorage, the actions come on.
  button(/^Sign in$/).click()
  await tick()
  await type(doc.querySelector('.auth input[type=password]'), 'tok-alice')
  button(/^Sign in$/, doc.querySelector('.auth')).click()
  await tick()
  assert.match(doc.querySelector('.auth').textContent, /signed in/)
  assert.equal(w.sessionStorage.getItem('stormdrive.bearer'), 'tok-alice')
  assert.ok(!doc.querySelector('.bulk fieldset').disabled)

  // Format → 4096… opens Prepare with the format step set.
  button(/Format → 4096…/, doc.querySelector('.bulk')).click()
  await tick()
  const pane = () => doc.querySelector('aside.pane')
  assert.match(pane().textContent, /Prepare[\s\S]*24 selected drive\(s\)/)
  assert.match(pane().textContent, /format → 4096/)
  assert.ok(button(/^Run on/, pane()).disabled, 'nothing runs before a preview')

  // Dry run: 22 runnable, sd0 holds a slab (needs its serial), sd1 busy.
  button(/Preview/, pane()).click()
  await tick(10)
  const dry = posts.find((p) => p.req.dry_run)
  assert.deepEqual(dry.req.select.drives.length, 24)
  assert.deepEqual(dry.req.steps, [{ op: 'format', block_size: 4096 }])
  assert.match(pane().textContent, /Runs on 22/)
  assert.match(pane().textContent, /Holds data \(1\)[\s\S]*sd0[\s\S]*a stormblock slab/)
  assert.match(pane().textContent, /Refused \(1\)[\s\S]*sd1 — busy: testing/)
  assert.match(button(/^Run on/, pane()).textContent, /Run on 22 drives/)

  // A /dev name does not count; the serial does.
  const held = pane().querySelector('.held input')
  await type(held, 'sd0')
  assert.match(button(/^Run on/, pane()).textContent, /Run on 22 drives/)
  await type(held, 'S0')
  assert.match(button(/^Run on/, pane()).textContent, /Run on 23 drives/)

  // Run: confirmed, sent with the bearer and destroy [S0].
  button(/^Run on/, pane()).click()
  await tick(20)
  assert.match(confirms.at(-1), /format → 4096 on 23 drive\(s\)[\s\S]*DESTROYS everything on 23 drive\(s\), 1 of them holding data you named/)
  const run = posts.find((p) => p.path === 'api/v1/worker/jobs' && !p.req.dry_run)
  assert.equal(run.auth, 'Bearer tok-alice')
  assert.deepEqual(run.req.destroy, ['S0'])
  assert.equal(pane(), null, 'the pane closes on submit')
  assert.match(text(), /job j1: 23 drive\(s\) queued/)

  // The Jobs panel: the job, sd0's progress, the refusal, Cancel offered.
  await tick(50)
  const panel = doc.querySelector('details.jobs')
  assert.match(panel.textContent, /Jobs \(1, 1 open\)/)
  assert.match(panel.textContent, /j1[\s\S]*format → 4096[\s\S]*1 running · 22 queued · 1 refused[\s\S]*kubernetes:alice/)
  assert.match(panel.textContent, /sd0[\s\S]*S0[\s\S]*running · format → 4096 10% · formatting/)
  assert.match(panel.textContent, /sd1[\s\S]*busy: testing/)
  assert.ok(!button(/^Cancel$/, panel).disabled)
  assert.ok(button(/^Resume$/, panel).disabled, 'nothing interrupted')
  button(/^Cancel$/, panel).click()
  await tick(10)
  assert.ok(posts.some((p) => p.path === 'api/v1/worker/jobs/j1/cancel' && p.auth === 'Bearer tok-alice'))

  // The state column carries the prep phase.
  top()[0].querySelector('button.expander').click()
  await tick()
  assert.match(doc.querySelector('.nested tbody tr').textContent, /prep\s*unusable/)

  // Sign out: read only again.
  button(/Sign out/).click()
  await tick()
  assert.equal(w.sessionStorage.getItem('stormdrive.bearer'), null)
  assert.match(text(), /read only/)
  w.close()
})
