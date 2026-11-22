const root = document.documentElement;
const reduceMotion = window.matchMedia('(prefers-reduced-motion: reduce)');
const finePointer = window.matchMedia('(hover: hover) and (pointer: fine)');

const THEME_KEY = 'pengudroid-theme';
const THEME_COLORS = { dark: '#070a09', light: '#eaf1ec' } as const;
type Theme = keyof typeof THEME_COLORS;

function applyTheme(theme: Theme) {
  root.dataset.theme = theme;
  root.style.colorScheme = theme;
  document.querySelector('meta[name="theme-color"]')?.setAttribute('content', THEME_COLORS[theme]);
  document.querySelectorAll<HTMLButtonElement>('[data-theme-toggle]').forEach((button) => {
    button.setAttribute('aria-label', theme === 'dark' ? 'Switch to light theme' : 'Switch to dark theme');
    button.setAttribute('aria-pressed', String(theme === 'light'));
  });
}

applyTheme((root.dataset.theme as Theme) ?? 'dark');

document.querySelectorAll<HTMLButtonElement>('[data-theme-toggle]').forEach((button) => {
  button.addEventListener('click', () => {
    const next: Theme = root.dataset.theme === 'light' ? 'dark' : 'light';
    applyTheme(next);
    try {
      localStorage.setItem(THEME_KEY, next);
    } catch {
      /* storage can be blocked; the theme still applies for this page */
    }
  });
});

/* Mobile menu ------------------------------------------------------------- */

const menuToggle = document.querySelector<HTMLButtonElement>('[data-menu-toggle]');
const menu = document.querySelector<HTMLElement>('[data-menu]');

function setMenu(open: boolean) {
  if (!menuToggle || !menu) return;
  menu.hidden = !open;
  menuToggle.setAttribute('aria-expanded', String(open));
  menuToggle.setAttribute('aria-label', open ? 'Close menu' : 'Open menu');
}

setMenu(false);

menuToggle?.addEventListener('click', () => setMenu(menu?.hidden ?? true));

menu?.addEventListener('click', (event) => {
  if ((event.target as HTMLElement).closest('a')) setMenu(false);
});

document.addEventListener('keydown', (event) => {
  if (event.key === 'Escape' && menu && !menu.hidden) {
    setMenu(false);
    menuToggle?.focus();
  }
});

document.addEventListener('click', (event) => {
  if (!menu || menu.hidden) return;
  const target = event.target as Node;
  if (menu.contains(target) || menuToggle?.contains(target)) return;
  setMenu(false);
});

window.addEventListener('resize', () => {
  if (window.innerWidth > 860 && menu && !menu.hidden) setMenu(false);
});

/* Reveal on scroll -------------------------------------------------------- */

const revealTargets = document.querySelectorAll<HTMLElement>('.reveal');

if (reduceMotion.matches || !('IntersectionObserver' in window)) {
  revealTargets.forEach((el) => el.classList.add('is-in'));
} else {
  const observer = new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        if (!entry.isIntersecting) continue;
        entry.target.classList.add('is-in');
        observer.unobserve(entry.target);
      }
    },
    { rootMargin: '0px 0px -8% 0px', threshold: 0.08 },
  );
  revealTargets.forEach((el) => observer.observe(el));
}

/* Pointer sheen ----------------------------------------------------------- */

if (finePointer.matches && !reduceMotion.matches) {
  for (const card of document.querySelectorAll<HTMLElement>('.sheen')) {
    card.addEventListener('pointermove', (event) => {
      const rect = card.getBoundingClientRect();
      card.style.setProperty('--mx', `${((event.clientX - rect.left) / rect.width) * 100}%`);
      card.style.setProperty('--my', `${((event.clientY - rect.top) / rect.height) * 100}%`);
    });
  }
}

/* Copy buttons ------------------------------------------------------------ */

async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    const area = document.createElement('textarea');
    area.value = text;
    area.setAttribute('readonly', '');
    area.style.position = 'fixed';
    area.style.opacity = '0';
    document.body.append(area);
    area.select();
    const execCommand = (document as unknown as { execCommand(commandId: string): boolean }).execCommand.bind(document);
    const ok = execCommand('copy');
    area.remove();
    return ok;
  }
}

document.querySelectorAll<HTMLButtonElement>('[data-copy]').forEach((button) => {
  button.addEventListener('click', async () => {
    const scope = button.closest('[data-code]') ?? document;
    const source = scope.querySelector<HTMLElement>('[data-code-text]');
    const text = source?.textContent?.trim() ?? '';
    if (!text) return;
    const ok = await copyText(text);
    const label = button.querySelector<HTMLElement>('[data-copy-label]');
    const previous = label?.textContent ?? 'Copy';
    button.dataset.copied = String(ok);
    if (label) label.textContent = ok ? 'Copied' : 'Press Ctrl+C';
    window.setTimeout(() => {
      button.dataset.copied = 'false';
      if (label) label.textContent = previous;
    }, 1800);
  });
});

/* Compatibility filters --------------------------------------------------- */

const filterButtons = [...document.querySelectorAll<HTMLButtonElement>('[data-filter]')];
const scenarios = [...document.querySelectorAll<HTMLElement>('[data-status]')];
const filterStatus = document.querySelector<HTMLElement>('[data-filter-status]');

if (filterButtons.length && scenarios.length) {
  const applyFilter = (value: string) => {
    let visible = 0;
    for (const scenario of scenarios) {
      const show = value === 'all' || scenario.dataset.status === value;
      scenario.hidden = !show;
      if (show) visible += 1;
    }
    for (const button of filterButtons) {
      button.setAttribute('aria-pressed', String(button.dataset.filter === value));
    }
    if (filterStatus) {
      filterStatus.textContent = `Showing ${visible} of ${scenarios.length} audited scenarios`;
    }
  };

  for (const button of filterButtons) {
    button.addEventListener('click', () => applyFilter(button.dataset.filter ?? 'all'));
  }

  const initial = filterButtons.find((button) => button.getAttribute('aria-pressed') === 'true');
  applyFilter(initial?.dataset.filter ?? 'all');
}

/* Docs text filter -------------------------------------------------------- */

const docInput = document.querySelector<HTMLInputElement>('[data-doc-filter]');
const docGroups = [...document.querySelectorAll<HTMLElement>('[data-doc-group]')];
const docEmpty = document.querySelector<HTMLElement>('[data-doc-empty]');

if (docInput) {
  const applyDocFilter = () => {
    const query = docInput.value.trim().toLowerCase();
    let matches = 0;
    for (const group of docGroups) {
      let groupMatches = 0;
      for (const item of group.querySelectorAll<HTMLElement>('[data-doc-item]')) {
        const haystack = `${item.dataset.docItem ?? ''} ${item.textContent ?? ''}`.toLowerCase();
        const show = query === '' || haystack.includes(query);
        item.hidden = !show;
        if (show) groupMatches += 1;
      }
      group.hidden = groupMatches === 0;
      matches += groupMatches;
    }
    if (docEmpty) docEmpty.hidden = matches !== 0;
  };

  docInput.addEventListener('input', applyDocFilter);
  applyDocFilter();
}

export {};
