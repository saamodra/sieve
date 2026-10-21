import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

// `npm run dev` proxies the API to a running `sieve start` or the demo example.
const backend = 'http://127.0.0.1:8731';

const loopbackOrigin = /^http:\/\/(localhost|127\.0\.0\.1|\[::1\])(:\d+)?$/;

export default defineConfig({
  plugins: [svelte()],
  server: {
    proxy: {
      '/api': {
        target: backend,
        ws: true,
        changeOrigin: true,
        // The backend refuses any Origin but its own. Vouch for pages served
        // by this dev server only; other origins pass through and get refused.
        // `bypass` runs before both HTTP and WebSocket requests are proxied,
        // and the proxy copies `req.headers`.
        bypass: (req) => {
          const origin = req.headers.origin;
          if (origin && loopbackOrigin.test(origin) && origin === `http://${req.headers.host}`) {
            req.headers.origin = backend;
          }
        },
      },
    },
  },
});
