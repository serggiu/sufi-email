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
  it("injects the CSP meta tag into a head", () => {
    const html = "<html><head><title>x</title></head><body>hi</body></html>";
    const out = withEmailCsp(html);
    expect(out).toContain('meta http-equiv="Content-Security-Policy"');
    expect(out).toContain("default-src 'none'; script-src 'none'; img-src data:");
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
  it("forbids all scripts (the parent app handles sizing and links)", () => {
    const out = withEmailCsp("<html><head></head><body>hi</body></html>");
    // No <script> element is injected, and the policy allows none.
    expect(out).not.toContain("<script");
    expect(out).toContain("script-src 'none'");
  });
  it("keeps the email's own scripts inert (no hash whitelist)", () => {
    const html = "<html><head></head><body><script>alert(1)</script></body></html>";
    const out = withEmailCsp(html);
    // The email's script stays in the document but the policy forbids it.
    expect(out).toContain("script-src 'none'");
    expect(out).toContain("<script>alert(1)</script>");
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
  it("injects a script-free policy (no inline handler to hash)", () => {
    const out = withEmailCsp("<html><head></head><body>hi</body></html>");
    expect(out).not.toContain("sha256-");
    expect(out).not.toContain("<script");
  });
});
