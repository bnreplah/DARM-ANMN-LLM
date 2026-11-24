// Offline shell only. The service worker caches the app's own static files
// and never touches requests to an archive, so it cannot see or alter ledger
// data, tokens or signed requests.
const CACHE = 'darm-companion-v1';
const SHELL = ['./', 'index.html', 'app.css', 'app.js', 'manifest.webmanifest', 'icons/icon.svg', 'icons/icon-192.png', 'icons/icon-512.png',
  'src/bytes.js', 'src/canonical.js', 'src/hash.js', 'src/merkle.js', 'src/json.js', 'src/identity.js', 'src/protocol.js', 'src/entries.js',
  'src/ledger.js', 'src/wallet.js', 'src/worker.js', 'src/store.js',
  'vendor/noble-hashes/_blake.js', 'vendor/noble-hashes/_md.js', 'vendor/noble-hashes/_u64.js', 'vendor/noble-hashes/blake2.js', 'vendor/noble-hashes/blake3.js', 'vendor/noble-hashes/utils.js'];

self.addEventListener('install', (e) => e.waitUntil(caches.open(CACHE).then((c) => c.addAll(SHELL)).then(() => self.skipWaiting())));
self.addEventListener('activate', (e) =>
  e.waitUntil(caches.keys().then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k)))).then(() => self.clients.claim())));
self.addEventListener('fetch', (e) => {
  const url = new URL(e.request.url);
  if (e.request.method !== 'GET' || url.origin !== self.location.origin || url.pathname.endsWith('config.json')) return;
  e.respondWith(fetch(e.request).then((res) => {
    if (res.ok) caches.open(CACHE).then((c) => c.put(e.request, res.clone()));
    return res;
  }).catch(() => caches.match(e.request)));
});
