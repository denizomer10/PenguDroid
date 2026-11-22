import { defineConfig } from 'astro/config';
import db from '@astrojs/db';
import sitemap from '@astrojs/sitemap';

// ASTRO_BASE=/PenguDroid for a GitHub Pages project site; '/' for local dev.
const base = process.env.ASTRO_BASE ?? '/';
const site = process.env.ASTRO_SITE ?? 'https://denizomer10.github.io';

export default defineConfig({
  site,
  base,
  output: 'static',
  integrations: [db(), sitemap()],
  build: {
    format: 'directory',
  },
});
