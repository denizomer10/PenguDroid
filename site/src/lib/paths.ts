const rawBase = import.meta.env.BASE_URL;

/** Normalized site base: always starts and ends with a slash ('/' or '/PenguDroid/'). */
export const base = rawBase.endsWith('/') ? rawBase : `${rawBase}/`;

/** Prefix an internal path with the configured base so GitHub Pages subpaths work. */
export function withBase(path: string): string {
  if (!path.startsWith('/')) return path;
  if (path === '/') return base;
  return `${base}${path.replace(/^\/+/, '')}`;
}

/** True when the current page matches the link, so the nav can mark it active. */
export function isActive(currentPath: string, href: string): boolean {
  const target = withBase(href);
  const current = currentPath.endsWith('/') ? currentPath : `${currentPath}/`;
  if (target === base) return current === base;
  return current === target || current.startsWith(target);
}
