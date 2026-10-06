// API base: direct (:9092/, /ui, /ui/) → '/', through stormd's proxy
// (/ui/proxy/stormdrive/ or /ui/ext/stormdrive/) → that prefix. The same
// pattern mkube uses; no redirects, which a proxied iframe cannot follow.
const m = location.pathname.match(/^(\/ui\/(?:proxy|ext)\/[^/]+\/)/)
export const API = m ? m[1] : '/'

// The bearer (#47, #19): every write on :9092 needs a storage-admin one since
// 0.18.0 (#45), and every read a storage-viewer one since 0.21.0 (#19).
// Pasted by the operator (`oc whoami -t`), kept for this tab only
// (sessionStorage), sent on every request.
const KEY = 'stormdrive.bearer'

export function bearer() {
  try {
    return sessionStorage.getItem(KEY) || ''
  } catch {
    return ''
  }
}

export function setBearer(token) {
  try {
    if (token) sessionStorage.setItem(KEY, token.trim())
    else sessionStorage.removeItem(KEY)
  } catch {}
}

/// An error from the API: the envelope's message, its HTTP status and
/// `code` (`unauthorized`, `forbidden`, `conflict`, …).
export class ApiError extends Error {
  constructor(message, status, code) {
    super(message)
    this.status = status
    this.code = code
  }
  /// Refused for want of (or by) a bearer.
  get auth() {
    return this.status === 401 || this.status === 403
  }
  /// No bearer, or one the node does not know: sign in.
  get signIn() {
    return this.status === 401
  }
}

async function errorOf(r) {
  let msg = `${r.status} ${r.statusText}`
  let code
  try {
    const j = await r.json()
    if (j.error) msg = j.error
    code = j.code
  } catch {}
  if (r.status === 401) msg = `sign in with a bearer: storage-viewer to read, storage-admin to change (${msg})`
  else if (r.status === 403) msg = `needs storage-admin: this bearer may not (${msg})`
  return new ApiError(msg, r.status, code)
}

function withAuth(opts = {}) {
  const t = bearer()
  if (!t) return opts
  return { ...opts, headers: { ...(opts.headers || {}), Authorization: `Bearer ${t}` } }
}

export async function api(path, opts) {
  const r = await fetch(API + path, withAuth(opts))
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
  const r = await fetch(
    API + `api/v1/firmware/images/${encodeURIComponent(name)}`,
    withAuth({ method: 'PUT', body: file, headers: { 'Content-Type': 'application/octet-stream' } }),
  )
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
