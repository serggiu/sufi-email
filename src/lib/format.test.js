import { describe, it, expect } from "vitest";
import { JSDOM } from "jsdom";
import {
  fmtDate,
  fmtSize,
  escapeHtml,
  withEmailCsp,
  LINK_HANDLER_CODE,
  EMAIL_BODY_STYLE,
} from "./format.js";

describe("fmtSize", () => {
  it("formats bytes", () => {
    expect(fmtSize(0)).toBe("0 B");
    expect(fmtSize(512)).toBe("512 B");
  });
  it("formats KB with one decimal", () => {
    expect(fmtSize(1024)).toBe("1.0 KB");
    expect(fmtSize(1536)).toBe("1.5 KB");
  });
  it("formats MB with one decimal", () => {
    expect(fmtSize(1024 * 1024)).toBe("1.0 MB");
    expect(fmtSize(3 * 1024 * 1024 + 512 * 1024)).toBe("3.5 MB");
  });
});

describe("escapeHtml", () => {
  it("escapes & < >", () => {
    expect(escapeHtml('a & b < c > d')).toBe("a &amp; b &lt; c &gt; d");
  });
  it("leaves plain text alone", () => {
    expect(escapeHtml("hello world")).toBe("hello world");
  });
});

describe("withEmailCsp", () => {
  it("injects the CSP meta tag into a head", () => {
    const html = "<html><head><title>x</title></head><body>hi</body></html>";
    const out = withEmailCsp(html);
    expect(out).toContain('meta http-equiv="Content-Security-Policy"');
    expect(out).toMatch(/default-src 'none'; script-src 'nonce-[0-9a-f]+'; img-src data:/);
    expect(out.indexOf("<head>") < out.indexOf("meta http-equiv")).toBe(true);
  });
  it("prepends the meta when there is no head", () => {
    const out = withEmailCsp("<body>hi</body>");
    expect(out.startsWith('<meta http-equiv="Content-Security-Policy"')).toBe(true);
  });
  it("blocks remote images in the policy", () => {
    const out = withEmailCsp("<html><head></head></html>");
    expect(out).toContain("img-src data:");
    expect(out).not.toContain("https:");
  });
  it("relaxes image/media/font loading only when remote images are opted in", () => {
    const out = withEmailCsp("<html><head></head><body>hi</body></html>", {
      remoteImages: true,
    });
    expect(out).toContain("img-src * data: blob:");
    expect(out).toContain("media-src * data: blob:");
    expect(out).toContain("font-src * data: blob:");
    // Scripts stay pinned to the nonce, connect stays blocked.
    expect(out).toMatch(/script-src 'nonce-[0-9a-f]+'/);
    expect(out).toContain("default-src 'none'");
    expect(out).not.toContain("connect-src");
  });
  it("defaults to the strict policy (no remote images)", () => {
    const out = withEmailCsp("<html><head></head><body>hi</body></html>");
    expect(out).toContain("img-src data:");
    expect(out).not.toContain("img-src *");
  });
  it("adds a no-referrer meta so opted-in image loads don't leak the app origin", () => {
    const out = withEmailCsp("<html><head></head></html>", { remoteImages: true });
    expect(out).toContain('<meta name="referrer" content="no-referrer">');
  });
  it("injects the email-body layout guardrails (images never overflow the pane)", () => {
    const out = withEmailCsp("<html><head></head><body>hi</body></html>");
    expect(out).toContain(`<style>${EMAIL_BODY_STYLE}</style>`);
    expect(EMAIL_BODY_STYLE).toContain("img,video{max-width:100%");
    expect(EMAIL_BODY_STYLE).toContain("table{max-width:100%}");
  });
  it("allows exactly one nonce'd script: the link handler", () => {
    const out = withEmailCsp("<html><head></head><body>hi</body></html>");
    // The injected handler script carries the same nonce as the policy.
    const nonce = /script-src 'nonce-([0-9a-f]+)'/.exec(out)[1];
    expect(out).toContain(`<script nonce="${nonce}">`);
    expect(out).toContain("sufi-open-url");
    expect(out).toContain("window.parent.postMessage");
    // And nothing else script-shaped is added by the wrapper.
    expect((out.match(/<script/g) || []).length).toBe(1);
  });
  it("uses a fresh nonce per message, so emails cannot guess it", () => {
    const a = withEmailCsp("<html><head></head><body>x</body></html>");
    const b = withEmailCsp("<html><head></head><body>x</body></html>");
    const na = /script-src 'nonce-([0-9a-f]+)'/.exec(a)[1];
    const nb = /script-src 'nonce-([0-9a-f]+)'/.exec(b)[1];
    expect(na).not.toBe(nb);
  });
  it("keeps the email's own scripts inert (they have no nonce)", () => {
    const html = "<html><head></head><body><script>alert(1)</script></body></html>";
    const out = withEmailCsp(html);
    // The email's script stays in the document but the policy forbids it
    // (no matching nonce); only the injected handler has the nonce.
    expect(out).toContain("<script>alert(1)</script>");
    const nonce = /script-src 'nonce-([0-9a-f]+)'/.exec(out)[1];
    expect(out).toContain(`<script nonce="${nonce}">`);
    expect(out).not.toContain(`<script nonce="${nonce}">alert(1)`);
  });
  it("inserts the handler inside the document so the parser runs it", () => {
    const out = withEmailCsp("<html><head></head><body>hi</body></html>");
    const bodyEnd = out.indexOf("</body>");
    const scriptAt = out.indexOf("<script nonce=");
    expect(scriptAt).toBeGreaterThan(-1);
    expect(scriptAt).toBeLessThan(bodyEnd);
  });
});

describe("fmtDate", () => {
  it("shows time for today, date otherwise", () => {
    const now = new Date();
    expect(fmtDate(now.toISOString())).not.toBe("");
    // A date far in the past yields a short date, not a time.
    const past = new Date("2020-01-02T10:00:00Z");
    expect(fmtDate(past.toISOString())).not.toMatch(/^\d{2}:\d{2}$/);
  });
  it("returns empty for missing input", () => {
    expect(fmtDate("")).toBe("");
    expect(fmtDate(null)).toBe("");
  });
});

describe("withEmailCsp link-handler policy", () => {
  it("pins the handler with a nonce instead of a hash", () => {
    const out = withEmailCsp("<html><head></head><body>hi</body></html>");
    expect(out).toContain("script-src 'nonce-");
    expect(out).not.toContain("sha256-");
  });
});

// Run the ACTUAL injected handler code in a fresh window and verify it
// forwards link clicks to the parent — the behavior the whole feature
// rests on, not just its presence in the markup.
describe("LINK_HANDLER_CODE behavior", () => {
  // A fresh window whose document already ran the real handler code, the
  // same way it runs inside a rendered email (as a <script> in the body).
  const freshWindow = () => {
    const dom = new JSDOM(
      "<!DOCTYPE html><html><body></body></html>" +
        `<script>${LINK_HANDLER_CODE}</script>`,
      {
        url: "https://app.local/",
        runScripts: "dangerously",
        // Keep jsdom's default action for javascript: links (an alert
        // dialog) from printing an unhandled-warning to the test output.
        beforeParse(win) {
          win.alert = () => {};
        },
      }
    );
    return dom.window;
  };
  const addLink = (win, href) => {
    const a = win.document.createElement("a");
    a.href = href;
    a.textContent = "link";
    win.document.body.appendChild(a);
    return a;
  };

  it("posts the http(s) URL to the parent on a left-click and prevents navigation", () => {
    const win = freshWindow();
    const posted = [];
    win.parent.postMessage = (msg, target) => posted.push({ msg, target });

    const a = addLink(win, "https://example.com/x");
    const ev = new win.MouseEvent("click", {
      bubbles: true,
      cancelable: true,
      button: 0,
    });
    a.dispatchEvent(ev);

    expect(posted).toEqual([
      { msg: { type: "sufi-open-url", url: "https://example.com/x" }, target: "*" },
    ]);
    expect(ev.defaultPrevented).toBe(true);
  });

  it("does not forward non-http(s) links", () => {
    const win = freshWindow();
    const posted = [];
    win.parent.postMessage = (msg, target) => posted.push({ msg, target });

    const a = addLink(win, "javascript:alert(1)");
    a.dispatchEvent(
      new win.MouseEvent("click", { bubbles: true, cancelable: true, button: 0 })
    );

    expect(posted).toEqual([]);
  });

  it("does not forward right-clicks or already-handled clicks", () => {
    const win = freshWindow();
    const posted = [];
    win.parent.postMessage = (msg, target) => posted.push({ msg, target });

    const a = addLink(win, "https://example.com/x");

    a.dispatchEvent(
      new win.MouseEvent("click", { bubbles: true, cancelable: true, button: 2 })
    );
    expect(posted).toEqual([]);

    const ev = new win.MouseEvent("click", {
      bubbles: true,
      cancelable: true,
      button: 0,
    });
    ev.preventDefault(); // someone already handled it
    a.dispatchEvent(ev);
    expect(posted).toEqual([]);
  });
});
