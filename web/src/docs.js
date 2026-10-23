import { mount } from 'svelte';
import { createApiReference } from '@scalar/api-reference';
import '@scalar/api-reference/style.css';
import '@fontsource-variable/inter';
import './tokens.css';
import './docs.css';
import DocsHeader from './DocsHeader.svelte';
// Importing the theme store keeps <html data-theme> in sync with the switcher and the OS.
import './theme.svelte.js';

mount(DocsHeader, { target: document.getElementById('docs-header') });

const mode = () => (document.documentElement.dataset.theme === 'dark' ? 'dark' : 'light');

const config = {
  url: '/api/openapi.json',
  // Send "Test Request" calls to the origin serving this page (BASE_URL in production; the Vite
  // dev server, which proxies /api, during development) instead of the spec's server URL.
  servers: [{ url: window.location.origin, description: 'This server' }],
  // Our palette lives in docs.css; disable Scalar's preset theme, fonts and toggle.
  theme: 'none',
  withDefaultFonts: false,
  hideDarkModeToggle: true,
  forceDarkModeState: mode(),
  darkMode: mode() === 'dark',
  // Self-hosted: no phoning home.
  telemetry: false,
  showDeveloperTools: 'never',
  mcp: { disabled: true },
  // Scalar's hosted AI chat would send the spec and questions to Scalar's servers.
  agent: { disabled: true },
  persistAuth: true,
  authentication: { preferredSecurityScheme: 'apiKey' },
  defaultHttpClient: { targetKey: 'shell', clientKey: 'curl' },
  metaData: { title: 'API Reference · Listaculous' },
  favicon: '/brand/favicon.png',
};

const reference = createApiReference('#api-reference', config);

// Follow the app's Light / System / Dark switch (and OS changes when on System). Scalar only reads
// the mode at startup and themes via body.light-mode / body.dark-mode, so we set those classes
// ourselves and also update its config so its own components agree.
function applyMode(next) {
  document.body.classList.toggle('dark-mode', next === 'dark');
  document.body.classList.toggle('light-mode', next !== 'dark');
}
let current = mode();
new MutationObserver(() => {
  const next = mode();
  if (next === current) return;
  current = next;
  reference.updateConfiguration({ ...config, forceDarkModeState: next, darkMode: next === 'dark' });
  applyMode(next);
}).observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] });
