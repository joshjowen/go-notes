/*
 * The service worker: makes the application itself survive a disconnection.
 *
 * Without this, offline mode covers everything except the case people actually
 * hit — closing the laptop, opening it somewhere with no network, and reloading
 * the tab. The notes would be in IndexedDB and completely unreachable, because
 * the browser could not fetch the HTML that loads the code that reads them.
 *
 * What it caches and why:
 *
 *   - Navigations get the network first, and the cached shell if that fails.
 *     Network-first rather than cache-first so a deployed update is picked up on
 *     the next load rather than whenever the cache happens to be invalidated.
 *   - Static assets are cached as they are used. Trunk fingerprints its output
 *     (`name-<16 hex>.js`), so a new build asks for new names and the old
 *     entries are swept on the next activation.
 *   - `/api/**` is never cached, ever. The application layer already knows how
 *     to work from its local copy and how to queue what it cannot send; a
 *     service worker replaying a stale note on top of that would be a second,
 *     invisible source of truth. Serving a cached `POST` response, or a cached
 *     `/api/me`, is exactly the kind of "helpful" that loses somebody's writing.
 *
 * Nothing here fetches from another origin. That is deliberate: Go-Notes has to
 * run on an air-gapped network, so every byte the page needs comes from the
 * server that served the page.
 */

const VERSION = 'go-notes-v1';
const SHELL = '/';

self.addEventListener('install', (event) => {
  event.waitUntil(
    caches
      .open(VERSION)
      .then((cache) => cache.add(SHELL))
      // A failed pre-cache must not abort the install: the worker is still
      // useful, it will simply fill on first use.
      .catch(() => undefined)
      .then(() => self.skipWaiting()),
  );
});

self.addEventListener('activate', (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((names) =>
        Promise.all(names.filter((name) => name !== VERSION).map((name) => caches.delete(name))),
      )
      .then(() => self.clients.claim()),
  );
});

self.addEventListener('fetch', (event) => {
  const request = event.request;

  if (request.method !== 'GET') return;

  const url = new URL(request.url);
  // Another origin has no business being cached by us, and on an air-gapped
  // network there is nothing there to cache.
  if (url.origin !== self.location.origin) return;

  // The API is the application's own business.
  if (url.pathname === '/healthz' || url.pathname.startsWith('/api/')) return;

  if (request.mode === 'navigate') {
    event.respondWith(networkFirst(request, SHELL));
    return;
  }

  // Only a name that carries a content hash may be served from the cache
  // without checking. The editor bundle is copied through under a fixed name,
  // so caching it blind would pin an editor bug in every returning browser
  // until someone cleared their site data by hand — the same trap the server's
  // `Cache-Control` logic sidesteps, and it has to be sidestepped twice.
  if (isFingerprinted(url.pathname)) {
    event.respondWith(cacheFirst(request));
    return;
  }

  event.respondWith(networkFirst(request, request));
});

/** Trunk emits `name-<16 hex digits>.ext` for everything it generates. */
function isFingerprinted(pathname) {
  return /-[0-9a-f]{16}\.[a-z0-9]+$/i.test(pathname);
}

/**
 * Fresh if we can get it, cached if we cannot.
 *
 * `fallbackKey` is what the response is stored under and what a failure falls
 * back to. For a navigation that is the shell: every client-side route
 * (`/note/Projects/A.md`) is served the same HTML, so one cached copy answers
 * all of them. For anything else it is the request itself.
 */
async function networkFirst(request, fallbackKey) {
  const cache = await caches.open(VERSION);
  try {
    const response = await fetch(request);
    if (response && response.ok) {
      cache.put(fallbackKey, response.clone());
    }
    return response;
  } catch (err) {
    const cached = (await cache.match(fallbackKey)) || (await cache.match(request));
    if (cached) return cached;
    throw err;
  }
}

/**
 * Cached if we have it, and fetched and kept if not.
 *
 * Safe for the WebAssembly bundle and the editor because their names change
 * when their contents do; `index.html` never reaches here, since navigations
 * are handled above.
 */
async function cacheFirst(request) {
  const cache = await caches.open(VERSION);
  const cached = await cache.match(request);
  if (cached) return cached;

  const response = await fetch(request);
  if (response && response.ok && response.type === 'basic') {
    cache.put(request, response.clone());
  }
  return response;
}
