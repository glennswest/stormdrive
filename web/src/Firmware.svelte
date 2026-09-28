<script>
  // The firmware image store: upload a vendor image, see what is stored,
  // delete one. Updating drives happens from the bulk bar or a drive pane.
  import { putImage, del } from './lib/api.js'
  import { human } from './lib/model.js'

  let { images = [], act, onerror } = $props()
  let file = $state(null)
  let info = $state('')

  async function upload() {
    const f = file?.[0]
    if (!f) return onerror('choose a file first')
    const name = f.name.replace(/[^A-Za-z0-9._+-]/g, '_')
    info = `uploading ${name} (${human(f.size)})…`
    try {
      const j = await putImage(name, f)
      info = `stored ${j.name} · sha256 ${j.sha256.slice(0, 12)}…`
      file = null
    } catch (e) {
      info = ''
      onerror(e.message || String(e))
    }
    act(Promise.resolve())
  }
</script>

<details>
  <summary>Firmware images ({images.length})</summary>
  <div class="row">
    {#each images as i}
      <span class="chip" title="sha256 {i.sha256}">{i.name} · {human(i.size)}</span>
      <button title="delete" onclick={() => confirm(`Delete image ${i.name} from the store?`) && act(del(`api/v1/firmware/images/${encodeURIComponent(i.name)}`))}>✕</button>
    {:else}
      <span class="dim">no images — upload a vendor .lod/.bin</span>
    {/each}
  </div>
  <div class="row">
    <input type="file" bind:files={file} />
    <button onclick={upload}>Upload</button>
    <span class="dim">{info}</span>
  </div>
</details>

<style>
  details { margin-top: 16px; background: var(--panel); border: 1px solid var(--border); border-radius: var(--radius); padding: 8px 12px; }
  summary { cursor: pointer; color: var(--text-dim); font-size: 12px; text-transform: uppercase; letter-spacing: 0.5px; }
  .row { display: flex; gap: 6px; flex-wrap: wrap; align-items: center; margin: 8px 0; }
  .chip { padding: 1px 8px; border-radius: 9px; background: var(--accent-bg); color: var(--accent); font-size: 12px; }
  .dim { color: var(--text-dim); font-size: 12px; }
</style>
