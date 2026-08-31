// Pure formatting helpers shared across views.

export function fmtDate(iso) {
  if (!iso) return "";
  const d = new Date(iso);
  const today = new Date();
  if (d.toDateString() === today.toDateString()) {
    return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  }
  return d.toLocaleDateString([], { month: "short", day: "numeric" });
}

export function fmtSize(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

export function escapeHtml(s) {
  return s
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;");
}

// The exact source of the inline link handler injected into every email
// body. It is the ONLY script the email's CSP allows (via a per-render
// nonce), so it can listen for clicks on links inside the message — a
// real click on a link is NOT delivered to a click listener that the host
// attaches to the frame document from the parent world (WebKitGTK only
// delivers it to listeners inside the frame's own document, i.e. this
// script) — and forward http(s) URLs to the host app, which opens them in
// the system browser. The email's own scripts have no nonce and stay
// blocked. The <script> element must carry the same nonce and this exact
// source text. Exported for tests.
export const LINK_HANDLER_CODE = `document.addEventListener("click", function (e) {
  var t = e.target;
  var a = t && t.closest ? t.closest("a[href]") : null;
  if (!a || e.defaultPrevented || e.button !== 0) return;
  var href = a.href || "";
  if (!/^https?:\\/\\//i.test(href)) return;
  e.preventDefault();
  window.parent.postMessage({ type: "sufi-open-url", url: href }, "*");
}, true);`;

// A fresh random nonce per rendered message, so the email's own scripts
// can never guess it (Web Crypto is available in the app's webview).
function emailNonce() {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

// Small layout guardrails injected into every email body. Newsletters and
// marketing mail rely on fixed-width tables and full-width images that
// overflow a narrow reading pane; these rules keep content inside the
// frame without re-theming the email (the sender's own CSS comes after
// this block in the document and still wins).
export const EMAIL_BODY_STYLE = `
img,video{max-width:100%;height:auto}
table{max-width:100%}
pre{white-space:pre-wrap;overflow-wrap:break-word}
body{overflow-wrap:break-word}`;

// The Content-Security-Policy injected into an email body document.
//
// Strict (default): no network at all — remote images (including tracking
// pixels) never load, only data: images (embedded cid: parts rewritten to
// data: URLs) and inline styles. Scripts are pinned to exactly ONE
// nonce'd handler.
//
// Remote-images-allowed (opt-in per message, see the banner in the
// message view): the email may fetch images, media and fonts from any
// origin so legit newsletters display fully — but nothing else changes:
// scripts stay pinned to the nonce'd handler, connect-src/frame-src/
// object-src stay 'none', so the email still cannot exfiltrate or run
// code. A no-referrer meta (injected alongside) keeps image loads from
// leaking the app's local origin.
function cspPolicy(remoteImages, nonce) {
  if (remoteImages) {
    return `default-src 'none'; script-src 'nonce-${nonce}'; img-src * data: blob:; media-src * data: blob:; font-src * data: blob:; style-src 'unsafe-inline'`;
  }
  return `default-src 'none'; script-src 'nonce-${nonce}'; img-src data:; style-src 'unsafe-inline'`;
}

// Wrap an email body document with the app's security + layout scaffold:
// the CSP meta tag, a no-referrer meta (so even opted-in remote image
// loads don't reveal the app's origin), the layout guardrails, and the
// nonce'd link-handler script.
//
// opts.remoteImages (default false) relaxes the CSP to allow remote
// images/media/fonts — call this ONLY after the user opted in for this
// message (see hasRemoteImages/placeholderRemoteImages in mailview.js).
export function withEmailCsp(html, opts = {}) {
  const nonce = emailNonce();
  const headExtras =
    `<meta http-equiv="Content-Security-Policy" content="${cspPolicy(
      opts.remoteImages,
      nonce
    )}">` +
    `<meta name="referrer" content="no-referrer">` +
    `<style>${EMAIL_BODY_STYLE}</style>`;
  const script = `<script nonce="${nonce}">${LINK_HANDLER_CODE}</script>`;
  const head = /<head[^>]*>/i.exec(html);
  let out;
  if (head) {
    out =
      html.slice(0, head.index + head[0].length) +
      headExtras +
      html.slice(head.index + head[0].length);
  } else {
    out = headExtras + html;
  }
  // The handler must sit INSIDE the document: content after </html> is
  // ignored by the parser and the script would never run.
  const bodyEnd = /<\/body\s*>/i.exec(out);
  if (bodyEnd) {
    return out.slice(0, bodyEnd.index) + script + out.slice(bodyEnd.index);
  }
  const htmlEnd = /<\/html\s*>/i.exec(out);
  if (htmlEnd) {
    return out.slice(0, htmlEnd.index) + script + out.slice(htmlEnd.index);
  }
  return out + script;
}
