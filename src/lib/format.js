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

// The exact source of the inline link handler (hashed into the CSP, so only
// this exact script may run — the email's own scripts have a different hash
// and stay blocked). The emitted <script> element must contain exactly this
// text between its tags, since CSP hashes cover that source text verbatim.
const LINK_HANDLER_CODE =
  'document.addEventListener("click", function (e) {\n' +
  '  var t = e.target;\n' +
  '  var a = t && t.closest ? t.closest("a[href]") : null;\n' +
  '  if (!a || e.defaultPrevented || e.button !== 0) return;\n' +
  '  var href = a.href || "";\n' +
  '  if (!/^https?:\\/\\//i.test(href)) return;\n' +
  '  e.preventDefault();\n' +
  '  window.parent.postMessage({ type: "sufi-open-url", url: href }, "*");\n' +
  '}, true);\n' +
  'function sufiReportSize() {\n' +
  '  var h = Math.max(document.body ? document.body.scrollHeight : 0, document.documentElement ? document.documentElement.scrollHeight : 0);\n' +
  '  window.parent.postMessage({ type: "sufi-frame-size", height: h }, "*");\n' +
  '}\n' +
  'window.addEventListener("load", sufiReportSize);\n' +
  'if (window.ResizeObserver) { new ResizeObserver(sufiReportSize).observe(document.body); }';

let linkHandlerHash = null;
async function linkHandlerSha256() {
  if (!linkHandlerHash) {
    const digest = await crypto.subtle.digest(
      "SHA-256",
      new TextEncoder().encode(LINK_HANDLER_CODE)
    );
    linkHandlerHash = btoa(String.fromCharCode(...new Uint8Array(digest)));
  }
  return linkHandlerHash;
}

// Allow only embedded (data:) images and inline styles; everything else —
// remote images, web fonts, scripts, frames — is blocked. The one exception
// is the link handler above, whose exact hash is pinned in the CSP, so HTML
// links inside the email can be forwarded to the host app to open in the
// external browser, while the email's own scripts stay blocked.
export async function withEmailCsp(html) {
  const hash = await linkHandlerSha256();
  const meta =
    `<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src data:; style-src 'unsafe-inline'; script-src 'sha256-${hash}'">`;
  // The script element's inner text must equal LINK_HANDLER_CODE exactly,
  // so the CSP hash matches (no added whitespace/newlines).
  const script = `<script>${LINK_HANDLER_CODE}</script>`;
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
  // Insert the handler INSIDE the document: content after </html> is
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
