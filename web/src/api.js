export async function api(path, { method = 'GET', body } = {}) {
  const res = await fetch(`/api${path}`, {
    method,
    headers: body !== undefined ? { 'content-type': 'application/json' } : {},
    body: body !== undefined ? JSON.stringify(body) : undefined,
    credentials: 'same-origin',
  });
  const text = await res.text();
  let data = null;
  try { data = text ? JSON.parse(text) : null; } catch { data = text; }
  if (!res.ok) {
    const err = new Error(data?.error || `Request failed (${res.status})`);
    err.status = res.status;
    if (res.status === 401 && path !== '/auth/login' && path !== '/auth/setup') window.dispatchEvent(new Event('lc:unauthorized'));
    throw err;
  }
  return data;
}

export function fmtDate(s) {
  return s ? new Date(s).toLocaleString() : '';
}
