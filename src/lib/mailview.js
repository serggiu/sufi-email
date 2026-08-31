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
// the body, from the fetched base64 payloads of the embedded parts. Keys
// are lowercased: Content-IDs are compared case-insensitively (headers and
// HTML references frequently disagree on case, e.g. `Content-ID: Logo@X`
// vs `src="cid:logo@x"`), matching the normalization cidTokensInHtml and
// the embedded-part plan already apply.
export function cidDataMap(cidParts, dataByPart) {
  const map = new Map();
  for (const p of cidParts) {
    const b64 = dataByPart.get(p.part_id);
    if (b64) map.set(String(p.contentId).toLowerCase(), `data:${p.contentType};base64,${b64}`);
  }
  return map;
}

// All `cid:` tokens referenced by an HTML email body, normalized (angle
// brackets and surrounding whitespace stripped, lowercased). Used to decide
// which attachment parts are genuinely embedded in the body (and should be
// rewritten to data: URLs) vs. plain attachments that merely happen to carry
// a Content-ID header — Gmail and Apple Mail add Content-IDs to ordinary
// attachments too.
//
// Covers every place a body can reference an embedded part: src/poster/
// background attributes, CSS url(cid:...) (inline styles), srcset
// candidates, and SVG image href/xlink:href. (<object data> is NOT
// handled: object-src 'none' blocks <object> regardless, so treating it
// as embedded would hide the part from the thumbnail list without ever
// rendering it.)
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
  rewriteMediaTags(html, (attrs) => {
    for (const [name, value] of attrs) {
      if (name === "srcset") {
        for (const url of srcsetCandidateUrls(value)) {
          const cid = /^cid:(.*)$/i.exec(url.trim());
          if (cid) add(cid[1]);
        }
      } else if (name === "href" || name === "xlink:href") {
        const cid = /^["']?cid:(.*?)["']?$/i.exec(String(value || "").trim());
        if (cid) add(cid[1]);
      }
    }
    return attrs;
  });
  return tokens;
}

// Parse the attribute list of a media tag into [name, value] pairs
// (lowercased names, quotes stripped; boolean attributes have value null).
// Media tags = img, image, source, video, object, embed.
function parseAttrs(s) {
  const attrs = [];
  const re = /([^\s=/>]+)(?:\s*=\s*("[^"]*"|'[^']*'|[^\s>]+))?/g;
  let m;
  while ((m = re.exec(s))) {
    const name = m[1];
    if (name === "/") continue;
    let value = m[2];
    if (value) value = value.replace(/^["']|["']$/g, "");
    attrs.push([name.toLowerCase(), value]);
  }
  return attrs;
}

function serializeAttrs(attrs) {
  if (!attrs.length) return "";
  return (
    " " +
    attrs
      .map(([name, value]) =>
        value == null ? name : `${name}="${String(value).replaceAll('"', "&quot;")}"`
      )
      .join(" ")
  );
}

// Run fn over the attributes of every media tag in the HTML and re-emit
// the tags. Used by cid rewriting and remote-image placeholding, where
// attribute juggling (srcset vs src, svg href, object data) is too
// fiddly for a single regex. Non-media tags are left untouched.
function rewriteMediaTags(html, fn) {
  return html.replace(
    /<(img|image|source|video|object|embed)\b([^>]*)>/gi,
    (tag, name, rest) => {
      const out = fn(parseAttrs(rest), tag);
      return `<${name}${serializeAttrs(out)}>`;
    }
  );
}

// The candidate URLs of a srcset attribute value (quotes stripped,
// descriptors like "1x" dropped, in document order).
function srcsetCandidateUrls(value) {
  return String(value || "")
    .trim()
    .replace(/^["']|["']$/g, "")
    .split(",")
    .map((c) => c.trim().split(/\s+/)[0])
    .filter(Boolean);
}

// Whether an HTML body references remote (http(s) or protocol-relative)
// images — src/poster/background attributes, CSS url(...), srcset
// candidates, and SVG image href. cid: and data: references
// never count. Used to decide when to offer the "Load remote images"
// banner.
export function hasRemoteImages(html) {
  if (!html) return false;
  const isRemote = (v) => /^(https?:)?\/\//i.test(String(v || "").trim());
  let found = false;
  html.replace(
    /(src|poster|background)\s*=\s*("[^"]*"|'[^']*'|[^\s>]+)/gi,
    (m, _attr, value) => {
      if (isRemote(value.replace(/^["']|["']$/g, ""))) found = true;
      return m;
    }
  );
  html.replace(/url\(\s*["']?((?:https?:)?\/\/[^"')]+)["']?\s*\)/gi, (m) => {
    found = true;
    return m;
  });
  rewriteMediaTags(html, (attrs) => {
    for (const [name, value] of attrs) {
      if (name === "srcset") {
        if (srcsetCandidateUrls(value).some(isRemote)) found = true;
      } else if (
        name === "src" ||
        name === "poster" ||
        name === "background" ||
        name === "href" ||
        name === "xlink:href"
      ) {
        if (isRemote(value)) found = true;
      }
    }
    return attrs;
  });
  return found;
}

// A 1x1 transparent GIF. Remote image references in a message the user
// has NOT opted to load are rewritten to this, so blocked images render
// as blank space instead of broken-image icons while the banner explains
// how to load the real ones.
const BLANK_GIF = "data:image/gif;base64,R0lGODlhAQABAAAAACH5BAEKAAEALAAAAAABAAEAAAICTAEAOw==";

// Replace every remote (http(s) / protocol-relative) image reference in
// an HTML body with the blank GIF, leaving cid: and data: references
// untouched. Mirrors hasRemoteImages' coverage. Exported for tests and
// used by the message view before rendering a body whose remote images
// are still blocked.
export function placeholderRemoteImages(html) {
  const isRemote = (v) => /^(https?:)?\/\//i.test(String(v || "").trim());
  let out = html.replace(
    /(src|poster|background)\s*=\s*("[^"]*"|'[^']*'|[^\s>]+)/gi,
    (m, attr, value) => {
      if (isRemote(value.replace(/^["']|["']$/g, ""))) return `${attr}="${BLANK_GIF}"`;
      return m;
    }
  );
  out = out.replace(
    /url\(\s*["']?((?:https?:)?\/\/[^"')]+)["']?\s*\)/gi,
    () => `url("${BLANK_GIF}")`
  );
  out = rewriteMediaTags(out, (attrs) => {
    const kept = [];
    for (const [name, value] of attrs) {
      if (name === "srcset") {
        // A data: URL breaks srcset's comma grammar, so a remote srcset is
        // dropped outright — the tag falls back to its src (already
        // blanked if remote) or its alt text.
        if (srcsetCandidateUrls(value).some(isRemote)) continue;
        kept.push([name, value]);
        continue;
      }
      if (
        (name === "src" ||
          name === "poster" ||
          name === "background" ||
          name === "href" ||
          name === "xlink:href") &&
        isRemote(value)
      ) {
        kept.push([name, BLANK_GIF]);
        continue;
      }
      kept.push([name, value]);
    }
    return kept;
  });
  return out;
}

// Replace `cid:` image references in an HTML email body with data: URLs, so
// embedded images render inside the sandboxed iframe (whose CSP only allows
// data: images). `cidMap` maps a Content-ID (without angle brackets, as
// reported by the backend) to a full data: URL. References without a matching
// part are left untouched.
export function rewriteCidImages(html, cidMap) {
  const lookup = (cid) => {
    const key = String(cid || "").trim().replace(/^<+|>+$/g, "").toLowerCase();
    return cidMap.get(key) || "";
  };
  let out = html
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
  // Media-tag attributes regexes cannot reach: srcset (data: URLs don't
  // survive srcset's comma grammar, so a cid srcset becomes a single
  // src) and SVG image href/xlink:href.
  out = rewriteMediaTags(out, (attrs) => {
    const srcset = attrs.find(([n]) => n === "srcset");
    if (srcset && srcset[1] && /\bcid:/i.test(srcset[1])) {
      const first = srcsetCandidateUrls(srcset[1]).find((u) => /^cid:/i.test(u));
      const url = first ? lookup(first.replace(/^cid:/i, "")) : "";
      if (url) {
        // Replace the whole srcset with a single data: src, dropping any
        // original src so the embedded image wins.
        return [["src", url], ...attrs.filter(([n]) => n !== "src" && n !== "srcset")];
      }
      // Unresolvable cid: drop the srcset so the tag falls back to its
      // own src (which the pass above already rewrote if it was cid:).
      return attrs.filter(([n]) => n !== "srcset");
    }
    for (const n of ["href", "xlink:href"]) {
      const a = attrs.find(([x]) => x === n);
      if (a && a[1] && /^["']?cid:/i.test(String(a[1]).trim())) {
        const url = lookup(String(a[1]).trim().replace(/^["']?cid:/i, ""));
        if (url) {
          return attrs.map(([x, v]) => (x === n ? [x, url] : [x, v]));
        }
      }
    }
    return attrs;
  });
  return out;
}

// Render inside a sandboxed iframe. The injected CSP meta tag allows exactly
// one script (the nonce'd link handler, see withEmailCsp) and blocks all
// network — most importantly remote images, so tracking pixels in HTML mail
// cannot phone home. The email's own scripts stay dead (no matching nonce).
// opts.remoteImages relaxes the CSP so remote images load — only call it
// after the user opted in (see the banner flow in app.js).
export async function renderViewBody(view, body, opts = {}) {
  const content =
    body.html ??
    `<pre style="white-space:pre-wrap;font:14px/1.5 monospace">${escapeHtml(
      body.text ?? "(empty message)"
    )}</pre>`;
  view.frame.srcdoc = await withEmailCsp(content, opts);
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
