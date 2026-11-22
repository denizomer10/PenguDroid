import type { ImageMetadata } from 'astro';
import bbr2 from '../assets/components-bbr2-20261116.png';
import genshin from '../assets/genshin-impact-7.2.0-rtx3080.png';
import newpipe from '../assets/newpipe-live.png';
import logo from '../assets/logo.png';

// Maps imageFile values stored in Astro DB to optimized local assets.
export const evidenceImages: Record<string, ImageMetadata> = {
  'components-bbr2-20261116.png': bbr2,
  'newpipe-live.png': newpipe,
  'genshin-impact-7.2.0-rtx3080.png': genshin,
};

export { logo };
