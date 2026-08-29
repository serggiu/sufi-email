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

// Replace `cid:` image references in an HTML email body with data: URLs, so
// embedded images render inside the sandboxed iframe (whose CSP only allows
// data: images). `cidMap` maps a Content-ID (without angle brackets, as
// reported by the backend) to a full data: URL. References without a matching
// part are left untouched.
export function rewriteCidImages(html, cidMap) {
  const lookup = (cid) => {
    const key = String(cid || "").trim().replace(/^<+|>+$/g, "");
    return cidMap.get(key) || "";
  };
  return html
    .replace(
      /(src|poster|background)\s*=\s*("[^"]*"|'[^']*'|[^\s>]+)/gi,
      (match, attr, value) => {
        const cid = /^["']?cid:(.*?)["']?$/i.exec(value.trim());
        if (!cid) return match;
        const url = lookup(cid[1]);
        return url ? `${attr}="${url}"` : match;
      }
    )
    .replace(/url\(\s*["']?cid:([^"')]+)["']?\s*\)/gi, (match, cid) => {
      const url = lookup(cid);
      return url ? `url("${url}")` : match;
    });
}

// Render inside a fully sandboxed iframe (no scripts, no same-origin).
// A CSP meta tag additionally blocks remote resources — most importantly
// remote images, so tracking pixels in HTML mail cannot phone home.
export async function renderViewBody(view, body) {
  const content =
    body.html ??
    `<pre style="white-space:pre-wrap;font:14px/1.5 monospace">${escapeHtml(
      body.text ?? "(empty message)"
    )}</pre>`;
  view.frame.srcdoc = await withEmailCsp(content);
}
