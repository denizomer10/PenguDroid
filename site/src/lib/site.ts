export const site = {
  name: 'PenguDroid',
  tagline: 'Android apps and ARM64 games on a Linux desktop.',
  description:
    'PenguDroid runs Android 17 / API 37 apps and ARM64 games on an x86_64 Linux desktop without a booted Android VM, container or root.',
  repo: 'https://github.com/denizomer10/PenguDroid',
  releases: 'https://github.com/denizomer10/PenguDroid/releases/latest',
  issues: 'https://github.com/denizomer10/PenguDroid/issues',
  discussions: 'https://github.com/denizomer10/PenguDroid/discussions',
  license: 'https://github.com/denizomer10/PenguDroid/blob/main/LICENSE',
  docs: 'https://github.com/denizomer10/PenguDroid/tree/main/docs',
  latestVersion: '0.3.0',
  asset: {
    appimage: 'PenguDroid-0.3.0-x86_64.AppImage',
    bytes: 467552704,
    sha256: 'dc3fa8c18b305dec2ecc256ad7ec68fd1f210c0501e14eafc8bbddbb11bef064',
  },
} as const;

export const navLinks = [
  { label: 'Overview', href: '/' },
  { label: 'Compatibility', href: '/compatibility' },
  { label: 'Install', href: '/install' },
  { label: 'Docs', href: '/docs' },
] as const;
