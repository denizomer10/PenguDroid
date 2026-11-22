import sharp from 'sharp';
import { writeFile } from 'node:fs/promises';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const logoPath = resolve(here, '../src/assets/logo.png');
const publicDir = resolve(here, '../public');

await sharp(logoPath).resize(180, 180, { fit: 'cover' }).png({ compressionLevel: 9 }).toFile(resolve(publicDir, 'apple-touch-icon.png'));
await sharp(logoPath).resize(64, 64, { fit: 'cover' }).png({ compressionLevel: 9 }).toFile(resolve(publicDir, 'favicon.png'));

const logo70 = await sharp(logoPath).resize(70, 70, { fit: 'contain', background: { r: 0, g: 0, b: 0, alpha: 0 } }).png().toBuffer();
const logoB64 = logo70.toString('base64');

const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="1200" height="630" viewBox="0 0 1200 630">
  <defs>
    <radialGradient id="auroraA" cx="18%" cy="8%" r="70%">
      <stop offset="0%" stop-color="#2ec47c" stop-opacity="0.42"/>
      <stop offset="100%" stop-color="#2ec47c" stop-opacity="0"/>
    </radialGradient>
    <radialGradient id="auroraB" cx="88%" cy="28%" r="60%">
      <stop offset="0%" stop-color="#27c8b0" stop-opacity="0.30"/>
      <stop offset="100%" stop-color="#27c8b0" stop-opacity="0"/>
    </radialGradient>
    <linearGradient id="glass" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0%" stop-color="#ffffff" stop-opacity="0.10"/>
      <stop offset="100%" stop-color="#ffffff" stop-opacity="0.03"/>
    </linearGradient>
    <linearGradient id="edge" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0%" stop-color="#ffffff" stop-opacity="0.34"/>
      <stop offset="100%" stop-color="#ffffff" stop-opacity="0.06"/>
    </linearGradient>
    <linearGradient id="emerald" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0%" stop-color="#6cf0a8"/>
      <stop offset="100%" stop-color="#0f9c5c"/>
    </linearGradient>
    <filter id="soft" x="-40%" y="-40%" width="180%" height="180%">
      <feGaussianBlur stdDeviation="48"/>
    </filter>
    <filter id="grain">
      <feTurbulence type="fractalNoise" baseFrequency="0.9" numOctaves="3" stitchTiles="stitch"/>
    </filter>
  </defs>

  <rect width="1200" height="630" fill="#070a09"/>
  <rect width="1200" height="630" fill="url(#auroraA)"/>
  <rect width="1200" height="630" fill="url(#auroraB)"/>
  <g filter="url(#soft)" opacity="0.55">
    <ellipse cx="210" cy="90" rx="260" ry="180" fill="#2ec47c" opacity="0.30"/>
    <ellipse cx="1040" cy="230" rx="240" ry="170" fill="#27c8b0" opacity="0.22"/>
  </g>

  <g opacity="0.035">
    <rect width="1200" height="630" filter="url(#grain)"/>
  </g>

  <g transform="translate(96 132)">
    <image x="0" y="0" width="70" height="70" href="data:image/png;base64,${logoB64}"/>
    <text x="92" y="46" font-family="Georgia, 'Times New Roman', serif" font-size="38" fill="#f1f6f2">PenguDroid</text>
  </g>

  <text x="94" y="282" font-family="Georgia, 'Times New Roman', serif" font-size="76" fill="#f1f6f2">Android apps and ARM64</text>
  <text x="94" y="366" font-family="Georgia, 'Times New Roman', serif" font-size="76" fill="#f1f6f2" font-style="italic">games on a Linux desktop.</text>

  <text x="96" y="436" font-family="'Inter Tight', Helvetica, Arial, sans-serif" font-size="23" fill="#b9cdc2">Android 17 / API 37 · x86_64 Linux · Wayland · no booted Android VM</text>

  <g transform="translate(96 500)">
    <rect x="0" y="0" width="27" height="27" rx="13.5" fill="#3ee08a" opacity="0.16"/>
    <circle cx="13.5" cy="13.5" r="4" fill="#3ee08a"/>
    <text x="42" y="19" font-family="'Inter Tight', Helvetica, Arial, sans-serif" font-size="18" fill="#c8d8ce">144 / 229 JNI slots wired</text>
    <text x="300" y="19" font-family="'Inter Tight', Helvetica, Arial, sans-serif" font-size="18" fill="#c8d8ce">14 audited scenarios: 7 · 4 · 3</text>
    <text x="620" y="19" font-family="'Inter Tight', Helvetica, Arial, sans-serif" font-size="18" fill="#c8d8ce">0 missing JNI slots</text>
  </g>

  <g transform="translate(880 96)">
    <rect x="0" y="0" width="224" height="86" rx="20" fill="url(#glass)" stroke="url(#edge)"/>
    <text x="22" y="36" font-family="'Inter Tight', Helvetica, Arial, sans-serif" font-size="14" fill="#8fa79a" letter-spacing="1.6">RELEASE</text>
    <text x="22" y="64" font-family="Georgia, 'Times New Roman', serif" font-size="26" fill="#f1f6f2">0.3.0 AppImage</text>
  </g>

  <g transform="translate(96 566)">
    <rect x="0" y="0" width="1008" height="1" fill="#ffffff" opacity="0.10"/>
  </g>
  <text x="96" y="604" font-family="'Inter Tight', Helvetica, Arial, sans-serif" font-size="15" fill="#7e968a">Marketing and documentation site · Apache-2.0 · Screenshots are specific test runs, not endorsements.</text>
</svg>`;

await writeFile(resolve(publicDir, 'og.svg'), svg, 'utf8');
await sharp(Buffer.from(svg)).png({ compressionLevel: 9, palette: false }).toFile(resolve(publicDir, 'og.png'));

const info = await sharp(resolve(publicDir, 'og.png')).metadata();
console.log(`og.png ${info.width}x${info.height}`);
