// Pure decision logic for the email link-opening path. app.js wires these
// to the Tauri opener plugin; keeping the logic here lets tests guard the
// exact conditions that have made links dead in the past (the about:blank
// load-handler bug, and unsafe/non-http(s) URLs).

// Should the parent act on this frame's load event? The email iframes are
// created empty and navigated to their srcdoc afterwards, and WebKitGTK
// fires a `load` event for BOTH documents — the initial about:blank one
// and the real about:srcdoc content. Only the latter carries the message;
// acting on the blank document measures nothing and attaches handlers to a
// document that is immediately discarded (that is exactly why links were
// dead).
export function shouldHandleFrameLoad(doc) {
  return !!doc && doc.URL !== "about:blank";
}

// The http(s) URL a "sufi-open-url" postMessage (sent by the nonce'd script
// injected into the email, see format.js LINK_HANDLER_CODE) asks to open,
// or null when the payload is not a link-open request or the URL is not
// http(s). Only http(s) is ever opened — never javascript:, mailto:, etc.
export function emailLinkUrlFromMessage(data) {
  if (!data || data.type !== "sufi-open-url") return null;
  const url = data.url;
  if (typeof url !== "string") return null;
  return /^https?:\/\//i.test(url) ? url : null;
}

// The http(s) URL a click event inside an email targets, or null when the
// click is not a plain left-click on a link, was already handled, or the
// href is not http(s). Used by the parent-side backstop listener; the
// nonce'd script inside the email is the primary handler.
export function hrefFromEmailClick(event) {
  if (!event || event.defaultPrevented || event.button !== 0) return null;
  const t = event.target;
  const a = t && t.closest ? t.closest("a[href]") : null;
  if (!a) return null;
  const href = a.href || "";
  return /^https?:\/\//i.test(href) ? href : null;
}
