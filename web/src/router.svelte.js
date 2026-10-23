// Minimal hash router: #/sites/3 -> { parts: ['sites', '3'] }
export const route = $state({ parts: [] });

function update() {
  route.parts = location.hash.replace(/^#\/?/, '').split('?')[0].split('/').filter(Boolean);
}
window.addEventListener('hashchange', update);
update();

export function go(path) {
  location.hash = '#' + path;
}
