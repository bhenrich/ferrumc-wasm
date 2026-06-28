import { sveltekit } from '@sveltejs/kit/vite';
import tailwindcss from '@tailwindcss/vite';
import { defineConfig } from 'vite';

// During `vite dev` the SPA runs on Vite's port; proxy the read-only data
// endpoints to a locally running ferrumc server so live data flows in dev too.
// The production bundle is same-origin and never touches this.
export default defineConfig({
  plugins: [tailwindcss(), sveltekit()],
  server: {
    proxy: {
      '/api': 'http://127.0.0.1:9090',
      '/events': {
        target: 'http://127.0.0.1:9090',
        changeOrigin: true,
        ws: false
      }
    }
  }
});
