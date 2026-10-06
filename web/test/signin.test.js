// The built page on a node where nothing answers anonymously but health
// (#19), in jsdom: every read without a bearer is 401, so the page opens its
// sign-in box instead of an error; once a bearer is pasted every request
// (reads too) carries it and the drives appear.
// Run after `npm run build`:  npm run test:page
import test from 'node:test'
import assert from 'node:assert/strict'
import { JSDOM } from 'jsdom'
import { fileURLToPath } from 'node:url'

const tick = (ms = 0) => new Promise((r) => setTimeout(r, ms))

test('a 401 on reads opens sign-in; the bearer then rides on every read', async () => {
  const dom = new JSDOM('<!doctype html><html><body><div id="app"></div></body></html>', {
    url: 'https://localhost:9092/', pretendToBeVisual: true,
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

  const drive = {
    id: 'd-0', name: 'sda', path: '/dev/sda', paths: ['/dev/sda'], kind: 'sata_hdd', model: 'WD20EFAX', serial: 'W0',
    firmware: '0A82', capacity_bytes: 2e12, block_size: 512, physical_block_size: 4096, usable: true, needs_reformat: false,
    membership: 'out', designation: 'none', activity: 'idle', overcommit: { enabled: false, ratio: 1 },
    health: { status: 'good', temperature_c: 30, media_errors: 0, messages: [] },
    location: { controller: { scsi_host: 'host0', pcie_addr: '0000:01:00.0', driver: 'mpt3sas' } },
    prep: { phase: 'ready', pct: null }, usage: null,
  }
  const data = {
    'api/v1/health': { status: 'ok', version: '0.21.0', node: 'test-node', writes: { gate: 'enforce' }, reads: { anonymous: false } },
    'api/v1/drives': { drives: [drive] },
    'api/v1/shelves': { shelves: [] },
    'api/v1/events': { events: [], latest_seq: 0 },
    'api/v1/firmware/images': { images: [] },
    'api/v1/hbas': { hbas: [] },
    'api/v1/worker/jobs': { jobs: [] },
  }
  const seen = []
  globalThis.fetch = async (url, opts = {}) => {
    const path = String(url).replace(/^\//, '').split('?')[0]
    const auth = opts.headers?.Authorization
    seen.push({ path, auth })
    if (path !== 'api/v1/health' && auth !== 'Bearer tok-viewer') {
      return { ok: false, status: 401, statusText: 'Unauthorized', json: async () => ({ error: 'a credential is required', code: 'unauthorized' }) }
    }
    return { ok: true, status: 200, statusText: 'OK', json: async () => structuredClone(data[path] ?? {}) }
  }

  const app = fileURLToPath(new URL('../dist/assets/app.js', import.meta.url))
  await import(app)
  await tick(100)
  const doc = w.document
  const auth = () => doc.querySelector('.auth')
  const button = (re, root = doc) => [...root.querySelectorAll('button')].find((b) => re.test(b.textContent))

  // No bearer: the sign-in box is open, no error banner, no drives.
  assert.ok(auth().querySelector('input[type=password]'), 'sign-in box not opened by a 401')
  assert.equal(doc.querySelector('.banner.error'), null)
  assert.match(doc.querySelector('.counts').textContent, /0 drives/)

  // Closing it leaves the prompt, not "signed in".
  button(/✕/, auth()).click()
  await tick()
  assert.match(auth().textContent, /sign in to see this node's drives/)
  button(/^Sign in$/, auth()).click()
  await tick()

  const input = auth().querySelector('input[type=password]')
  input.value = 'tok-viewer'
  input.dispatchEvent(new w.Event('input', { bubbles: true }))
  await tick()
  seen.length = 0
  button(/^Sign in$/, auth()).click()
  await tick(50)
  assert.match(auth().textContent, /signed in/)
  assert.match(doc.querySelector('.counts').textContent, /1 drives/)
  const reads = seen.filter((s) => s.path !== 'api/v1/health')
  assert.ok(reads.length >= 6 && reads.every((s) => s.auth === 'Bearer tok-viewer'), JSON.stringify(seen))
  dom.window.close()
})
