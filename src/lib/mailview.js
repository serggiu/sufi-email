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

// Decide how each attachment of a message is displayed:
//  - cidParts: image parts whose content-id the HTML body actually
//    references via a cid: URL — they render inside the body (rewritten to
//    data: URLs) and get no thumbnail.
//  - thumbParts: image parts not shown in the body (unreferenced or
//    content-id-less), small enough to preview.
//  - wantParts: deduplicated union of both, in a stable order, for the
//    single batched data fetch.
export function planAttachmentDisplay(html, atts, limits = {}) {
  const maxCidBytes = limits.maxCidBytes ?? 8 * 1024 * 1024;
  const maxPreviewBytes = limits.maxPreviewBytes ?? 3 * 1024 * 1024;
  const embedded = html ? cidTokensInHtml(html) : new Set();
  const isImage = (a) => (a.contentType || "").startsWith("image/");
  const cidParts = atts.filter(
    (a) =>
      a.contentId &&
      embedded.has(String(a.contentId).toLowerCase()) &&
      isImage(a) &&
      a.size <= maxCidBytes
  );
  const thumbParts = atts.filter(
    (a) =>
      !embedded.has(String(a.contentId || "").toLowerCase()) &&
      isImage(a) &&
      a.size <= maxPreviewBytes
  );
  const wantParts = [...cidParts, ...thumbParts].filter(
    (p, i, arr) => arr.findIndex((q) => q.part_id === p.part_id) === i
  );
  return { cidParts, thumbParts, wantParts };
}

// Build the content-id → data: URL map used to rewrite cid: references in
// the body, from the fetched base64 payloads of the embedded parts.
export function cidDataMap(cidParts, dataByPart) {
  const map = new Map();
  for (const p of cidParts) {
    const b64 = dataByPart.get(p.part_id);
    if (b64) map.set(p.contentId, `data:${p.contentType};base64,${b64}`);
  }
  return map;
}

// All `cid:` tokens referenced by an HTML email body, normalized (angle
// brackets and surrounding whitespace stripped, lowercased). Used to decide
// which attachment parts are genuinely embedded in the body (and should be
// rewritten to data: URLs) vs. plain attachments that merely happen to carry
// a Content-ID header — Gmail and Apple Mail add Content-IDs to ordinary
// attachments too.
export function cidTokensInHtml(html) {
  const tokens = new Set();
  const add = (tok) => {
    const t = String(tok || "").trim().replace(/^<+|>+$/g, "").toLowerCase();
    if (t) tokens.add(t);
  };
  html.replace(
    /(src|poster|background)\s*=\s*("[^"]*"|'[^']*'|[^\s>]+)/gi,
    (m, _attr, value) => {
      const cid = /^["']?cid:(.*?)["']?$/i.exec(value.trim());
      if (cid) add(cid[1]);
      return m;
    }
  );
  html.replace(/url\(\s*["']?cid:([^"')]+)["']?\s*\)/gi, (m, cid) => {
    add(cid);
    return m;
  });
  return tokens;
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
