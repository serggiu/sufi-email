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

// Render inside a sandboxed iframe. The injected CSP meta tag allows exactly
// one script (the nonce'd link handler, see withEmailCsp) and blocks all
// network — most importantly remote images, so tracking pixels in HTML mail
// cannot phone home. The email's own scripts stay dead (no matching nonce).
export async function renderViewBody(view, body) {
  const content =
    body.html ??
    `<pre style="white-space:pre-wrap;font:14px/1.5 monospace">${escapeHtml(
      body.text ?? "(empty message)"
    )}</pre>`;
  view.frame.srcdoc = await withEmailCsp(content);
}

// The sandbox flags for the email-body iframe. Two flags are required:
//
//  - allow-same-origin: the host reads the message document from the parent
//    to size the frame (and as a click-handler backstop).
//  - allow-scripts: it lets the injected link-handler script (see
//    withEmailCsp) run inside the email. This is the ONLY reliable way to
//    catch real clicks on links in WebKitGTK: a click on a link is not
//    delivered to a listener the parent attaches to a JS-created srcdoc
//    iframe's document (verified on webkit2gtk-4.1, 2.52.6), but it IS
//    delivered to a script running inside the frame's own document. The
//    handler forwards http(s) URLs to the host via postMessage.
//
// allow-scripts does NOT let the email run arbitrary code: the injected CSP
// allows exactly one script — the handler — pinned by a fresh random nonce
// the email cannot know; the email's own scripts, inline event handlers and
// javascript: URLs stay blocked. Forms, popups and navigation stay blocked
// by the flags we deliberately omit (no allow-forms, allow-popups,
// allow-top-navigation).
//
// Regression guard for the reading-pane fix: sizing depends on the parent
// being able to read the frame's document, which requires allow-same-origin.
// The previous approach (allow-scripts + a hash-pinned script inside the
// email posting its height) never ran in the release webview, leaving every
// message in a fixed ~150px box.
export const EMAIL_FRAME_SANDBOX = "allow-same-origin allow-scripts";

// The content height of an email document, measured from the parent: the
// taller of the body and documentElement scroll heights. 0 when the
// document is not (yet) readable.
export function frameContentHeight(doc) {
  return Math.max(
    doc && doc.body ? doc.body.scrollHeight : 0,
    doc && doc.documentElement ? doc.documentElement.scrollHeight : 0
  );
}

// The height an email-body iframe should be, given the content height its
// document reported and the visible height of its scroll container.
//
// The iframe must never be SHORTER than its content: a shorter iframe puts
// a nested scrollbar inside the message body — the "fixed height window"
// bug where you have to scroll inside the message to read it. So content
// taller than the pane keeps its full height and the OUTER container (the
// preview pane / modal thread) scrolls instead. Content shorter than the
// available space fills the entire pane, so a message always uses the full
// reading area. The 80px floor keeps tiny/empty bodies visible.
export function frameDisplayHeight(contentHeight, availableHeight) {
  return Math.max(contentHeight, availableHeight, 80);
}

// The visible height an email-body iframe can occupy: the client height of
// its scroll container — the preview pane in the three-column view, the
// modal thread in the full-width modal. Falls back to the window height
// when the frame is not (yet) inside either container.
export function frameAvailableHeight(frame) {
  const container = frame.closest("#preview-pane, #modal-thread");
  return container ? container.clientHeight : window.innerHeight;
}
