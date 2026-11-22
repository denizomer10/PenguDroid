import type { FaqSeed, ReleaseSeed } from './types';

export const faqs: FaqSeed[] = [
  {
    question: 'Do I need a booted Android VM, container or root?',
    answer:
      'No. PenguDroid runs as a per-user Linux service and needs no Android virtual machine, container, root access or Android kernel. It does need an x86_64 Linux host with a compatible kernel, a Wayland session and working host GPU drivers.',
    sortOrder: 1,
  },
  {
    question: 'Does FUSE or fusermount have to be installed?',
    answer:
      'No. The 0.3.0 AppImage does not require FUSE or fusermount. Runtime component storage is kept in a normal per-user data directory.',
    sortOrder: 2,
  },
  {
    question: 'How much disk space does the first setup need?',
    answer:
      'Plan for at least 5 GiB free. The 0.3.0 AppImage itself is about 468 MB, and first-run setup unpacks and prepares the runtime components locally.',
    sortOrder: 3,
  },
  {
    question: 'What does the compatibility figure actually measure?',
    answer:
      'It measures JNI wiring plus a fixed 14-scenario foundation checklist — seven passed, four partial, three missing. It is not a whole-Android completion percentage; the audit index leaves that value null, and a passed scenario covers only its named regression test.',
    sortOrder: 4,
  },
  {
    question: 'Is the published AppImage the independent Pengu runtime?',
    answer:
      'No. The published 0.3.0 AppImage is the ART compatibility profile. The independent Pengu profile — its own DEX interpreter, JNI layer and builtins, with no ART or framework binaries — is built separately with --pengu and is still experimental.',
    sortOrder: 5,
  },
  {
    question: 'Does the NewPipe result prove the independent DexVM works?',
    answer:
      'No. NewPipe v0.29.1 ran through the hosted ART path. Its Java/Kotlin code needs no ARM64 translation, but it is not evidence for the independent DexVM.',
    sortOrder: 6,
  },
  {
    question: 'Will Genshin Impact or Zenless Zone Zero work for me?',
    answer:
      'There is no current compatibility claim for either. The Genshin Impact 7.2.0 screenshots are historical NVIDIA evidence, excluded from 0.3.0 validation. Zenless Zone Zero 3.3.0 is not working independently: login, UI and gameplay remain unverified.',
    sortOrder: 7,
  },
  {
    question: 'Are Play services, billing, DRM and cameras implemented?',
    answer:
      'No. They are not full implementations. WebView is served by the runtime Chromium provider as a compatibility path rather than an isolated renderer architecture, and hardware video decode is not yet a Codec2 component.',
    sortOrder: 8,
  },
];

const releasesUrl = 'https://github.com/denizomer10/PenguDroid/releases';

export const releases: ReleaseSeed[] = [
  {
    version: '0.3.0',
    name: 'Component runtime',
    status: 'latest',
    summary:
      'ART-dependent component profile in a portable AppImage: offline first-run setup, hosted Android 17 / API 37 framework components and the BBR2 validation record.',
    date: '2026-11-16',
    href: releasesUrl,
    sortOrder: 1,
  },
  {
    version: '0.2.0',
    name: 'Ubuntu validation',
    status: 'previous',
    summary:
      'Earlier validation on Ubuntu 26.04 with an NVIDIA RTX 3080, including the historical Genshin Impact capture that is excluded from 0.3.0 scope.',
    date: '2026-11-11',
    href: `${releasesUrl}/tag/v0.2.0`,
    sortOrder: 2,
  },
];
