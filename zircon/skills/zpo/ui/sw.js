// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// zpo's service worker: keeps a copy of the page so a reload still shows the
// UI, with its "lost the connection" banner, while the zpo server is down.
//
// Only responses with zpo's `X-Zpo: 1` header count as the server's: a proxy
// in front of zpo answers with its own error page (a 502 or 503) when zpo is
// down, and that must not replace the page. The cache is named after this
// build's page (the server fills in the version), so a new zpo build
// replaces the copy.

const CACHE = 'zpo-shell-__ZPO_SHELL_VERSION__';
// The page, and the server info it shows in banners (the machine's name).
const SHELL = '/';
const INFO = '/api/info';

self.addEventListener('install', () => self.skipWaiting());

self.addEventListener('activate', e => {
  e.waitUntil((async () => {
    for (const k of await caches.keys()) {
      if (k.startsWith('zpo-shell-') && k !== CACHE) await caches.delete(k);
    }
    await self.clients.claim();
  })());
});

// Network first; a response from zpo refreshes the copy. The copy answers only
// when zpo cannot be reached: no network, or a gateway error (5xx) from a proxy
// in front of zpo. Any other answer without zpo's header comes from another app
// on this origin (or a proxy's sign-in page) and is passed through; for the page
// itself this worker also unregisters, so it never keeps standing in for that app.
async function networkThenCopy(request, key) {
  const cache = await caches.open(CACHE);
  let r;
  try {
    r = await fetch(request);
  } catch (err) {
    const copy = await cache.match(key);
    if (copy) return copy;
    throw err;
  }
  if (r.headers.get('X-Zpo') === '1') {
    if (r.ok) await cache.put(key, r.clone());
    return r;
  }
  if (r.status >= 500) return (await cache.match(key)) || r;
  if (r.ok && key === SHELL) await self.registration.unregister();
  return r;
}

self.addEventListener('fetch', e => {
  const url = new URL(e.request.url);
  if (url.origin !== self.location.origin || e.request.method !== 'GET') return;
  if (e.request.mode === 'navigate' && (url.pathname === '/' || url.pathname === '/index.html')) {
    e.respondWith(networkThenCopy(e.request, SHELL));
  } else if (url.pathname === INFO) {
    e.respondWith(networkThenCopy(e.request, INFO));
  }
});
