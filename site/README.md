# PenguDroid website

The marketing and documentation site for [PenguDroid](https://github.com/denizomer10/PenguDroid): a Linux runtime for
Android apps and ARM64 games on x86_64 desktops. It is a static Astro site with a dark-first "liquid glass" design, a
light "frosted daylight" theme, and content served from Astro DB at build time.

## Stack

- **Astro 7.4** with `output: 'static'`; no client framework.
- **@astrojs/db 0.21.3** for the content model. The upstream package is deprecated and no longer actively developed; it
  still builds here, but all reads go through `src/lib/queries.ts` (which also re-exports `db`) so it can be replaced
  without touching pages.
- **@fontsource** variable Inter Tight and Instrument Serif, self-hosted. No Google Fonts or CDN requests.
- **sharp** for image optimisation through Astro's `<Image>`.
- Vanilla TypeScript for theme, navigation, filters, reveal and copy buttons. No React/Vue/Svelte/Tailwind.
- `esbuild` is an explicit devDependency because `@astrojs/db` imports it during config loading.

Requires Node 22+ (developed on Node 26) and npm 11.

## Commands

```bash
npm install
npm run dev       # http://localhost:4321
npm run check     # astro check + TypeScript diagnostics
npm run build     # seeds Astro DB, renders static pages into dist/
npm run preview   # serve dist/ locally
npm run verify    # assert the built HTML contains all DB-backed content
npm run data      # regenerate src/data/compatibility.generated.ts from the audit snapshot
npm run assets    # regenerate favicon, apple-touch-icon and og.png from the logo
```

There is deliberately no database push script: Astro 7 has no `astro db push`. The integration creates
`.astro/content.db`, recreates the tables and runs `db/seed.ts` automatically during `astro build` and on the first
request to `astro dev`.

## Data model

`db/config.ts` defines seven tables and `db/seed.ts` inserts every row:

| Table | Contents |
| --- | --- |
| `SiteStat` | Headline numbers (229 JNI slots, 144 wired, 14 scenarios, 71 missing signatures, platform facts) |
| `CompatibilityScenario` | The 14 audited foundation scenarios with status and evidence |
| `MissingCall` | All 71 observed missing Java signatures (a lower bound) |
| `AppEvidence` | BBR2, NewPipe and historical Genshin evidence cards |
| `DocumentLink` | 21 links into the PenguDroid documentation, grouped by category |
| `Faq` | Eight answered questions |
| `Release` | The 0.3.0 release and its status |

Content flow:

```text
src/data/source/compatibility-index.json  (verbatim PenguDroid audit snapshot)
        │  npm run data
        ▼
src/data/compatibility.generated.ts  ─┐
src/data/{stats,evidence,docs,faq}.ts ─┼─► db/seed.ts ─► .astro/content.db ─► src/lib/queries.ts ─► pages
                                       ┘
```

The audit snapshot and all copy live inside this repository. Nothing is imported from a parent directory at build or
run time.

## Structure

```text
db/                  Astro DB schema and seed
public/               favicon, apple-touch-icon, og.png
scripts/              data generator, asset generator, build verifier
src/assets/           logo and screenshots (optimised at build time)
src/components/       glass UI, architecture diagram, evidence cards, copy blocks
src/data/             typed content modules and the audit snapshot
src/layouts/          BaseLayout: head meta, theme bootstrap, nav, footer
src/lib/              queries, base-path helpers, image map, site constants
src/pages/            /, /compatibility, /install, /docs, /404, robots.txt
src/scripts/main.ts   theme, menu, filters, copy, reveal, pointer sheen
src/styles/global.css design tokens, glass surfaces, fallbacks, responsive layout
```

## Deployment

The output is fully static. Set two environment variables when the site is served from a subpath (for example GitHub
Pages at `/PenguDroid/`):

```bash
ASTRO_SITE=https://denizomer10.github.io ASTRO_BASE=/PenguDroid/ npm run build
```

- `ASTRO_SITE` defaults to `https://denizomer10.github.io`.
- `ASTRO_BASE` defaults to `/`. It is normalised to start and end with a slash.
- Internal links and assets pass through `withBase()` in `src/lib/paths.ts`; canonical, Open Graph, sitemap and
  `robots.txt` follow the same values.
- Deploy `dist/` to any static host. No server runtime is needed.

## Assets and provenance

The logo and screenshots come from the PenguDroid repository (`logo.png` and `docs/screenshots/`). They are committed
here as source assets and converted to responsive WebP at build time. Genshin Impact 7.2.0 is historical NVIDIA evidence
and is labelled as such. Beach Buggy Racing 2, NewPipe, Genshin Impact, Android, NVIDIA, Intel and other marks belong to
their owners; screenshots show specific test runs and imply no endorsement. No APK files are hosted here.

`npm run assets` rebuilds `favicon.png`, `apple-touch-icon.png` and the 1200×630 `og.png` from `src/assets/logo.png`.

## Content honesty

The compatibility section reports a **scoped audit**: JNI wiring plus a fixed 14-scenario foundation checklist. It is not
a whole-Android completion percentage, and the source index intentionally has `whole_android_compatibility_percent: null`.
Wired handlers may be partial or no-ops; a passed scenario covers only its named regression test. The 71 missing Java
signatures are observed and therefore a lower bound. App evidence is labelled by execution path: BBR2 2026.10.26 is the
packaged ART component profile, NewPipe uses hosted ART (not the independent DexVM), and Genshin 7.2.0 is historical and
out of 0.3.0 scope. ZZZ 3.3.0 is presented as not working independently. Keep these rules when editing copy.

## License

Apache-2.0. See `LICENSE` and `NOTICE`.
