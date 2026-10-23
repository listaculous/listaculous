// Theme preference: 'light' | 'dark' | 'system' (default). The resolved theme is written to
// <html data-theme>, which the CSS tokens key off. index.html applies it before first paint.
const KEY = 'lc-theme';
const media = window.matchMedia('(prefers-color-scheme: dark)');

function read() {
  try {
    const v = localStorage.getItem(KEY);
    return v === 'light' || v === 'dark' ? v : 'system';
  } catch {
    return 'system';
  }
}

export const theme = $state({ mode: read() });

function apply() {
  const dark = theme.mode === 'dark' || (theme.mode === 'system' && media.matches);
  document.documentElement.dataset.theme = dark ? 'dark' : 'light';
}

export function setTheme(mode) {
  theme.mode = mode;
  try {
    if (mode === 'system') localStorage.removeItem(KEY);
    else localStorage.setItem(KEY, mode);
  } catch {}
  apply();
}

media.addEventListener('change', apply);
apply();
