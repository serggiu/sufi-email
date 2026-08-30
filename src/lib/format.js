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

// The CSP meta tag injected into every email body document: no scripts at
// all, no network, only data: images and inline styles. Remote images are
// blocked (no tracking pixels). Links inside the email are handled by the
// host app, which attaches its own click listener to the iframe's document
// (the email is sandboxed same-origin so the parent can reach it) — no
// script needs to run inside the email, which keeps the sandbox airtight.
export function withEmailCsp(html) {
  const meta =
    `<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'none'; img-src data:; style-src 'unsafe-inline'">`;
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
  return out;
}
