import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

// `npm run dev` proxies the API to a running `sieve start` or the demo example.
const backend = 'http://127.0.0.1:8081';

export default defineConfig({
  plugins: [svelte()],
  server: {
    proxy: {
      '/api': { target: backend, ws: true },
    },
  },
});
