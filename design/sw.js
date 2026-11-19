// App shell only: offline splash. /api/ always hits the network.
const C = 'devq-shell-v1';
const SHELL = ['/', '/icon.svg', '/icon-192.png', '/icon-512.png'];
self.addEventListener('install', e => e.waitUntil(caches.open(C).then(c => c.addAll(SHELL)).then(() => self.skipWaiting())));
self.addEventListener('activate', e => e.waitUntil(caches.keys().then(ks => Promise.all(ks.filter(k => k !== C).map(k => caches.delete(k)))).then(() => self.clients.claim())));
self.addEventListener('fetch', e => {
  const u = new URL(e.request.url);
  if (e.request.method !== 'GET' || u.origin !== location.origin || u.pathname.startsWith('/api/')) return;
  if (!SHELL.includes(u.pathname)) return;
  e.respondWith(fetch(e.request).then(r => {
    if (r.ok) { const c = r.clone(); e.waitUntil(caches.open(C).then(x => x.put(e.request, c))); }
    return r;
  }).catch(() => caches.match(e.request, { ignoreSearch: true })));
});
