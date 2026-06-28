import adapter from '@sveltejs/adapter-static';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

/** @type {import('@sveltejs/kit').Config} */
const config = {
  preprocess: vitePreprocess(),
  kit: {
    // Static SPA: emit a hashed asset bundle plus an index.html shell that the
    // Rust binary serves via ServeDir. `fallback` makes every unknown path boot
    // the same shell so client-side panel state survives a hard refresh.
    adapter: adapter({
      pages: '../dist',
      assets: '../dist',
      fallback: 'index.html',
      precompress: false,
      strict: false
    })
  }
};

export default config;
