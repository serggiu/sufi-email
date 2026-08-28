// Shared message-details view rendering.
//
// Both the three-column preview and the full-width modal render a message
// through these functions, so a future change to the message view updates
// both places. A "view" is just the set of target elements
// ({ subject, meta, frame, ... }), which keeps this pure and testable with
// plain objects.

import { escapeHtml, withEmailCsp } from "./format.js";

export function renderViewSubject(view, summary) {
  view.subject.textContent = summary.subject || "(no subject)";
}

export function renderViewMeta(view, summary) {
  view.meta.textContent = [
    summary.from || "",
    summary.date ? new Date(summary.date).toLocaleString() : "",
  ]
    .filter(Boolean)
    .join("  ·  ");
}

// Render inside a fully sandboxed iframe (no scripts, no same-origin).
// A CSP meta tag additionally blocks remote resources — most importantly
// remote images, so tracking pixels in HTML mail cannot phone home.
export function renderViewBody(view, body) {
  const content =
    body.html ??
    `<pre style="white-space:pre-wrap;font:14px/1.5 monospace">${escapeHtml(
      body.text ?? "(empty message)"
    )}</pre>`;
  view.frame.srcdoc = withEmailCsp(content);
}
