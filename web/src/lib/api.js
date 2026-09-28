// API base: direct (:9092/, /ui, /ui/) → '/', through stormd's proxy
// (/ui/proxy/stormdrive/ or /ui/ext/stormdrive/) → that prefix. The same
// pattern mkube uses; no redirects, which a proxied iframe cannot follow.
const m = location.pathname.match(/^(\/ui\/(?:proxy|ext)\/[^/]+\/)/)
export const API = m ? m[1] : '/'

async function errorOf(r) {
  let msg = `${r.status} ${r.statusText}`
  try {
    const j = await r.json()
    if (j.error) msg = j.error
  } catch {}
  return new Error(msg)
}

export async function api(path, opts) {
  const r = await fetch(API + path, opts)
  if (!r.ok) throw await errorOf(r)
  return r.json()
}

export const get = (path) => api(path)
export const del = (path) => api(path, { method: 'DELETE' })
export const post = (path, body) =>
  api(path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body || {}),
  })

export async function putImage(name, file) {
  const r = await fetch(API + `api/v1/firmware/images/${encodeURIComponent(name)}`, {
    method: 'PUT',
    body: file,
    headers: { 'Content-Type': 'application/octet-stream' },
  })
  if (!r.ok) throw await errorOf(r)
  return r.json()
}

/// Run `fn` over `items`, at most `limit` at once. Resolves to
/// [{item, ok, error}] in input order — a bulk action reports every drive.
export async function each(items, limit, fn) {
  const out = new Array(items.length)
  let next = 0
  async function worker() {
    while (next < items.length) {
      const i = next++
      try {
        await fn(items[i])
        out[i] = { item: items[i], ok: true }
      } catch (e) {
        out[i] = { item: items[i], ok: false, error: e.message || String(e) }
      }
    }
  }
  await Promise.all(Array.from({ length: Math.min(limit, items.length) }, worker))
  return out
}
