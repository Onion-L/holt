# Holt landing site

Astro + React build of the Holt landing pages. One design per page, with
markup and CSS rendered statically by Astro:

- `/` — dark Astronaut landing (`src/components/HeroController.tsx`)
- `/changelog` — dark release index (`src/components/ChangelogController.tsx`)
- `/download` — per-platform downloads, Coast road layout
  (`src/components/DownloadController.tsx`)
- `/og` — 1200×630 dark Astronaut OG capture template, `noindex`

`/` and `/og` share the astronaut artwork. `/changelog` uses the same dark
palette, typography, and minimal navigation. `/download` keeps the Coast road
layout.

The React controllers render nothing; they resolve the version and download
links from the latest GitHub release. `src/components/detect.ts` holds their
shared platform and chip detection. There is no variant switcher.

## Commands

```sh
npm run dev       # local dev server
npm run build     # production build to dist/
npm run preview   # serve the build (serves stale files across rebuilds —
                  # restart it, or serve dist/ with a plain static server)
```

## Deploy

`wrangler.jsonc` (this directory, `assets.directory: ./dist`) serves the build
as static assets of the `holt` Worker. Cloudflare Workers Builds deploys it —
the Git connection configured in the dashboard runs `npm ci && npm run build`
in `site/` and then `npx wrangler deploy` on `main`. No local wrangler needed,
and every other branch gets a preview URL commented on its pull request.

## OG image

Capture a new `public/assets/og.png` (served at `/assets/og.png`) from the
built `/og` page:

```sh
npm run build && (python3 -m http.server 8124 -d dist &)
"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
  --headless=new --hide-scrollbars --window-size=1200,630 \
  --screenshot=public/assets/og.png \
  "http://localhost:8124/og/index.html"
npm run build   # ship the new image
```

The og/twitter meta on `/` points at the absolute
`https://holt.onionl5236.workers.dev/assets/og.png` URL — crawlers ignore
relative URLs.
