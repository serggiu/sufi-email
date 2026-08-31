// Remote-image consent + the opt-in banner, extracted from app.js so the
// flow is unit-testable (jsdom). The host app passes its real
// localStorage, account/folder names, DOM, and a re-render callback; the
// pure helpers here take a Storage-like object so tests can use a mock.
//
// Remote images are blocked by default (the email CSP forbids all network,
// so tracking pixels cannot phone home); the banner offers two opt-ins,
// both persisted locally:
//  - "Load images": just this message (keyed by account/folder/uid).
//  - "Always load images": every message, from now on.

export const REMOTE_IMAGES_ALWAYS_KEY = "sufi-remote-images-always";

// localStorage key for one message's consent. The key is never parsed
// back, so account/folder separators inside the values are harmless.
export function remoteImagesKey(accountName, folder, uid) {
  return `sufi-remote-images:${accountName}:${folder}:${uid}`;
}

export function remoteImagesAlways(storage) {
  try {
    return storage.getItem(REMOTE_IMAGES_ALWAYS_KEY) === "1";
  } catch (_) {
    return false;
  }
}

// Whether a message's remote images may load: the global "always" flag
// wins; otherwise the per-message consent. Any storage failure (e.g. the
// webview denying access) falls back to blocked.
export function remoteImagesAllowed(storage, accountName, folder, uid) {
  if (remoteImagesAlways(storage)) return true;
  try {
    return storage.getItem(remoteImagesKey(accountName, folder, uid)) === "1";
  } catch (_) {
    return false;
  }
}

export function rememberRemoteImages(storage, accountName, folder, uid) {
  try {
    storage.setItem(remoteImagesKey(accountName, folder, uid), "1");
  } catch (_) {}
}

export function rememberRemoteImagesAlways(storage) {
  try {
    storage.setItem(REMOTE_IMAGES_ALWAYS_KEY, "1");
  } catch (_) {}
}

// Build and wire the opt-in banner above a message body whose remote
// images are blocked. Inserts the bar into `container` before `before`
// (the body iframe). Clicking either button records the consent and calls
// opts.render() — the host re-renders the frame with the relaxed CSP and
// the real URLs; the bar is removed either way.
//
// opts:
//   storage     Storage-like object (localStorage in the app)
//   accountName account the message belongs to
//   folder      folder the message belongs to
//   uid         message uid
//   render      () => void — re-render with remote images allowed
//   doc         Document to create elements in (defaults to the global)
export function buildRemoteImageBar(container, before, uid, opts) {
  const doc = opts.doc || document;
  const bar = doc.createElement("div");
  bar.className = "remote-bar";
  const note = doc.createElement("span");
  note.className = "remote-bar-note";
  note.textContent = "Remote images are blocked to protect your privacy.";
  const loadBtn = doc.createElement("button");
  loadBtn.className = "remote-bar-btn";
  loadBtn.textContent = "Load images";
  const alwaysBtn = doc.createElement("button");
  alwaysBtn.className = "remote-bar-btn";
  alwaysBtn.textContent = "Always load images";
  bar.append(note, loadBtn, alwaysBtn);
  container.insertBefore(bar, before);

  const reload = () => {
    if (opts.render) opts.render();
    bar.remove();
  };
  loadBtn.addEventListener("click", () => {
    rememberRemoteImages(opts.storage, opts.accountName, opts.folder, uid);
    reload();
  });
  alwaysBtn.addEventListener("click", () => {
    rememberRemoteImagesAlways(opts.storage);
    reload();
  });
  return bar;
}
