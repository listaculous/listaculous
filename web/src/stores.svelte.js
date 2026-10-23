import { api } from './api.js';

// Shared navigation data so the sidebar updates when sites/lists change.
export const nav = $state({ sites: [], lists: [] });

export async function refreshNav() {
  try {
    const [sites, dash] = await Promise.all([api('/sites'), api('/dashboard')]);
    nav.sites = sites;
    nav.lists = dash.lists;
  } catch {}
}

export function initials(name = '') {
  return name.split(/\s+/).filter(Boolean).slice(0, 2).map((w) => w[0].toUpperCase()).join('') || '?';
}
