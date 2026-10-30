# site — Astro + React landing page

- Four pages (`/`, `/changelog`, `/download`, `/og`) render static Astro
  HTML; React controllers (`src/components/*Controller.tsx`) render nothing
  and only resolve version and download links from the latest GitHub release.
  `src/components/detect.ts` holds their shared platform and chip detection.
  Keep the markup static unless asked otherwise — hydrating it with React
  changes text rasterization.
- Page CSS lives verbatim in each `.astro` page inside `<style is:inline>`
  (unprocessed).
- Static assets live in `site/public/assets/` (served at `/assets/…`; use
  absolute paths from pages — both `/changelog` and `/changelog/` resolve).
- `wrangler.jsonc` (this directory) deploys `dist/`. The deploy runs on
  Cloudflare's Workers Builds (the dashboard's Git connection: root `site`,
  build `npm ci && npm run build`, deploy `npx wrangler deploy`) — there is no
  deploy workflow in `.github/workflows/`, and branches other than `main` build
  as previews that the Cloudflare GitHub App links on the pull request.
  Building locally requires Node ≥ 22.12.
