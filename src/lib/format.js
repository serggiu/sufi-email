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

// Allow only embedded (data:) images and inline styles; everything else —
// remote images, web fonts, scripts, frames — is blocked.
export function withEmailCsp(html) {
  const meta =
    '<meta http-equiv="Content-Security-Policy" content="default-src \'none\'; img-src data:; style-src \'unsafe-inline\'">';
  const head = /<head[^>]*>/i.exec(html);
  if (head) {
    return (
      html.slice(0, head.index + head[0].length) +
      meta +
      html.slice(head.index + head[0].length)
    );
  }
  return meta + html;
}
