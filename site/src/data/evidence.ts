import type { AppEvidenceSeed } from './types';

const docs = 'https://github.com/denizomer10/PenguDroid/blob/main/docs';

export const appEvidence: AppEvidenceSeed[] = [
  {
    slug: 'bbr2',
    app: 'Beach Buggy Racing 2',
    version: '2026.10.26',
    headline: 'The final 0.3.0 AppImage stayed alive for 160 s and presented 5,911 frames.',
    summary:
      'Game and menu rendering plus injected tap/key input were verified in the packaged component profile, without a physical Android system tree. The live-race frame beside this copy comes from the preceding development build.',
    caveat:
      'Hosted ART component profile. Not an FPS benchmark; later game updates, other GPUs and the older 2026.10.05 APKM are not covered.',
    imageFile: 'components-bbr2-20261116.png',
    imageAlt:
      'Beach Buggy Racing 2 race in progress in a PenguDroid window, showing a red buggy, race HUD and an FPS counter.',
    pathLabel: 'ART · component runtime',
    docsHref: `${docs}/COMPONENT_APPIMAGE_TESTS.md`,
    sortOrder: 1,
  },
  {
    slug: 'newpipe',
    app: 'NewPipe',
    version: 'v0.29.1',
    headline: 'Live feed and video playback through the hosted ART path.',
    summary:
      'Network-fetched thumbnails render in the live feed. Its Java/Kotlin code needs no ARM64 translation; playback uses the hosted Android media framework and its software decoder.',
    caveat:
      'Hosted ART, not independent DexVM. Google Play services, billing and DRM are not full implementations.',
    imageFile: 'newpipe-live.png',
    imageAlt: 'NewPipe live feed running in PenguDroid, listing livestreams with thumbnail placeholders.',
    pathLabel: 'Hosted ART · software video',
    docsHref: `${docs}/HARDWARE_VIDEO.md`,
    sortOrder: 2,
  },
  {
    slug: 'genshin',
    app: 'Genshin Impact',
    version: '7.2.0',
    headline: 'Historical NVIDIA evidence: in-world gameplay on an RTX 3080.',
    summary:
      'Captured from a real desktop window with ARM64 Digitalis translation on an RTX 3080 Founders Edition and Core i3-12100F. Excluded from the 0.3.0 validation scope.',
    caveat:
      'Historical. Not a current compatibility claim for 0.3.0, later game updates, other GPUs or account flows.',
    imageFile: 'genshin-impact-7.2.0-rtx3080.png',
    imageAlt:
      'Genshin Impact gameplay in PenguDroid, showing a character in a grassy landscape with the party HUD visible.',
    pathLabel: 'Historical · ART evidence',
    docsHref: `${docs}/GENSHIN.md`,
    sortOrder: 3,
  },
];
