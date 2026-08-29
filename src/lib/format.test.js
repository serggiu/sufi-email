import { describe, it, expect } from "vitest";
import { fmtDate, fmtSize, escapeHtml, withEmailCsp } from "./format.js";

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
  it("injects the CSP meta tag into a head", async () => {
    const html = "<html><head><title>x</title></head><body>hi</body></html>";
    const out = await withEmailCsp(html);
    expect(out).toContain('meta http-equiv="Content-Security-Policy"');
    expect(out).toContain("default-src 'none'; img-src data:");
    expect(out.indexOf("<head>") < out.indexOf("meta http-equiv")).toBe(true);
  });
  it("prepends the meta when there is no head", async () => {
    const out = await withEmailCsp("<body>hi</body>");
    expect(out.startsWith('<meta http-equiv="Content-Security-Policy"')).toBe(true);
  });
  it("blocks remote images in the policy", async () => {
    const out = await withEmailCsp("<html><head></head></html>");
    expect(out).toContain("img-src data:");
    expect(out).not.toContain("https:");
  });
  it("allows only the hash-pinned link handler to run", async () => {
    const out = await withEmailCsp("<html><head></head><body>hi</body></html>");
    expect(out).toContain("script-src 'sha256-");
    expect(out).toContain("sufi-open-url");
    // The CSP pins the exact handler source by hash.
    const hash = out.match(/script-src 'sha256-([^']+)'/)[1];
    expect(hash.length).toBeGreaterThan(10);
    // Email scripts are not allowed via unsafe-inline.
    expect(out).not.toContain('script-src \'unsafe-inline\'');
  });
  it("places the handler inside the document, before </body>", async () => {
    const out = await withEmailCsp("<html><head></head><body>hi</body></html>");
    const bodyEnd = out.indexOf("</body>");
    const scriptStart = out.indexOf("<script");
    expect(scriptStart).toBeGreaterThan(-1);
    expect(scriptStart).toBeLessThan(bodyEnd);
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

describe("withEmailCsp hash integrity", () => {
  it("pins exactly the bytes emitted between <script> tags", async () => {
    const out = await withEmailCsp("<html><head></head><body>hi</body></html>");
    const cspHash = out.match(/script-src 'sha256-([^']+)'/)[1];
    const m = out.match(/<script>([\s\S]*?)<\/script>/);
    const inner = m[1];
    const digest = await crypto.subtle.digest(
      "SHA-256",
      new TextEncoder().encode(inner)
    );
    const computed = btoa(String.fromCharCode(...new Uint8Array(digest)));
    expect(computed).toBe(cspHash);
  });
});
