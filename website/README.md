# sieve website

Docs and landing page for sieve, built with [Astro Starlight](https://starlight.astro.build) and deployed to https://sieve.bysm.dev.

```sh
npm ci
npm run dev        # http://localhost:4321
npm run build      # outputs to dist/
npm run preview    # serve dist/ locally (search only works here)
```

- Docs pages are Markdown files in `src/content/docs/`. The sidebar is set in `astro.config.mjs`.
- The landing page is `src/pages/index.astro`.
- Design direction (palette, type, logo) is in `DESIGN.md`.
- Deploy: Cloudflare, root directory `website`, build command `npm run build`, output directory `dist`.
