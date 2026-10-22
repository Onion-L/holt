# site — Astro + React landing page

- Four pages (`/` Billboard, `/changelog` and `/download` Coast road, `/og`
  OG capture template) render the chosen prototype variant's markup verbatim
  as static Astro HTML; React controllers (`src/components/*Controller.tsx`)
  render nothing and only resolve version and download links from the latest
  GitHub release. `src/components/detect.ts` holds their shared platform and
  chip detection. Keep rendered output byte-identical to the prototypes unless
  asked otherwise — hydrating the markup itself with React changes text
  rasterization.
- The five-variant prototypes stay in `site/prototypes/` for reference; the
  variant switcher is not shipped.
- Page CSS lives verbatim in each `.astro` page inside `<style is:inline>`
  (unprocessed, so it stays byte-identical to the prototypes).
- Static assets live in `site/public/assets/` (served at `/assets/…`; use
  absolute paths from pages — both `/changelog` and `/changelog/` resolve).
- `wrangler.jsonc` (this directory) deploys `dist/`; the `deploy` workflow
  runs `npm ci && npm run build` first. Building locally requires Node ≥ 22.12.
