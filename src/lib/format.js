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

// The CSP meta tag injected into every email body document: no network
// (remote images are blocked, so tracking pixels cannot phone home), only
// data: images and inline styles — and exactly ONE script: the injected
// link handler, pinned by a random nonce. The email's own scripts have no
// nonce and stay blocked, as do inline event handlers and javascript:
// URLs (both count as scripts under script-src). Links inside the email
// are therefore handled only by the injected handler, which forwards
// http(s) clicks to the host app to open in the system browser.
export function withEmailCsp(html) {
  const nonce = emailNonce();
  const meta =
    `<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'nonce-${nonce}'; img-src data:; style-src 'unsafe-inline'">`;
  const script = `<script nonce="${nonce}">${LINK_HANDLER_CODE}</script>`;
  const head = /<head[^>]*>/i.exec(html);
  let out;
  if (head) {
    out =
      html.slice(0, head.index + head[0].length) +
      meta +
      html.slice(head.index + head[0].length);
  } else {
    out = meta + html;
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
