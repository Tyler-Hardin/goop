// goop service worker — caches the app shell for offline use.
const CACHE = "goop-v2";
const SHELL = ["/manifest.json", "/icon-192.png", "/icon-512.png"];

self.addEventListener("install", (e) => {
  e.waitUntil(
    caches.open(CACHE).then((cache) => cache.addAll(SHELL)),
  );
  self.skipWaiting();
});

self.addEventListener("activate", (e) => {
  e.waitUntil(
    caches.keys().then((keys) =>
      Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))),
    ),
  );
  self.clients.claim();
});

self.addEventListener("fetch", (e) => {
  if (e.request.method !== "GET") return;
  const url = new URL(e.request.url);
  if (url.origin !== self.location.origin) return;

  // API calls are never cached — they must always hit the network.
  if (url.pathname.startsWith("/api/")) {
    e.respondWith(fetch(e.request));
    return;
  }

  // Trunk builds produce hashed filenames (e.g. goop_web-abc123.js).
  // These are immutable, so cache-first is safe.  We also cache the
  // shell assets (icons, manifest) that are preloaded on install.
  const cacheable =
    /\.(?:js|wasm|css|png|ico)$/.test(url.pathname) ||
    url.pathname === "/manifest.json";

  if (cacheable) {
    e.respondWith(
      caches.match(e.request).then(
        (cached) =>
          cached ||
          fetch(e.request).then((resp) => {
            if (resp.ok) {
              const clone = resp.clone();
              caches.open(CACHE).then((cache) => cache.put(e.request, clone));
            }
            return resp;
          }),
      ),
    );
    return;
  }

  // Everything else (including the root page) — network-first so we
  // never serve stale content, but fall back to cache when offline.
  e.respondWith(
    fetch(e.request)
      .then((resp) => {
        if (resp.ok) {
          const clone = resp.clone();
          caches.open(CACHE).then((cache) => cache.put(e.request, clone));
        }
        return resp;
      })
      .catch(() => caches.match(e.request)),
  );
});

// ── push notifications ────────────────────────────────────────

self.addEventListener("push", (event) => {
  let data = {};
  try {
    if (event.data) data = event.data.json();
  } catch (_) {}

  const session = data.session || "goop";
  const body =
    data.event === "FinalResponse"
      ? "Prompt completed"
      : data.event === "Error"
        ? "Prompt errored"
        : data.event === "Cancelled"
          ? "Prompt cancelled"
          : "Prompt finished";

  const title = `goop — ${session}`;
  const options = {
    body,
    icon: "/icon-192.png",
    badge: "/icon-192.png",
    tag: `goop-${session}`,
    data: { session },
    requireInteraction: false,
  };

  event.waitUntil(self.registration.showNotification(title, options));
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const session = event.notification.data?.session;
  const target = session
    ? `/#session=${encodeURIComponent(session)}`
    : "/";

  event.waitUntil(
    clients.matchAll({ type: "window", includeUncontrolled: true }).then((windows) => {
      // Try to focus an existing window showing goop.
      for (const w of windows) {
        if (w.url.startsWith(self.location.origin)) {
          // Try to navigate to the target session.
          w.postMessage({ type: "goop-navigate", session });
          w.focus();
          return;
        }
      }
      // No existing window — open a new one.
      return clients.openWindow(target);
    }),
  );
});
