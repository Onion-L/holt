# Holt landing site

Astro + React build of the City Pop landing pages, from the variant prototypes
in `prototypes/` (kept for reference; not deployed). One design shipped per
page, markup and CSS verbatim from the chosen variant:

- `/` — hero, Billboard (`src/components/HeroController.tsx`)
- `/changelog` — Coast road layout (`src/components/ChangelogController.tsx`)
- `/og` — 1200×630 OG card capture template (Billboard), `noindex`

The React controllers render nothing; they resolve the version and dmg links
from the latest GitHub release, like the prototypes' scripts. The prototype
variant switcher (bottom bar, `?variant=`, ←/→) is intentionally not shipped.

## Commands

```sh
npm run dev       # local dev server
npm run build     # production build to dist/
npm run preview   # serve the build (serves stale files across rebuilds —
                  # restart it, or serve dist/ with a plain static server)
```

## Deploy

`wrangler.jsonc` (this directory, `assets.directory: ./dist`) serves the build as static assets of the `holt`
Worker. The `deploy` workflow builds on push to `main` and deploys; no local
wrangler needed.

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
