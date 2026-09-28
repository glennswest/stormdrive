import { defineConfig } from 'vite'
import { svelte } from '@sveltejs/vite-plugin-svelte'

// The page is served at /, /ui, /ui/ and through stormd's
// /ui/proxy/stormdrive/, so asset URLs are relative ('./assets/…') and the
// API base is worked out at runtime (src/lib/api.js). Output names are fixed
// (no content hashes) so src/api/mod.rs can include_str! them and the diff
// of the committed web/dist stays readable.
export default defineConfig({
  base: './',
  plugins: [svelte()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    rollupOptions: {
      output: {
        entryFileNames: 'assets/app.js',
        chunkFileNames: 'assets/[name].js',
        assetFileNames: 'assets/app[extname]',
      },
    },
  },
})
