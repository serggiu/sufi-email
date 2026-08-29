import { describe, it, expect } from "vitest";
import {
  renderViewSubject,
  renderViewMeta,
  renderViewBody,
  rewriteCidImages,
  cidTokensInHtml,
} from "./mailview.js";

function mockView() {
  return {
    subject: { textContent: "" },
    meta: { textContent: "" },
    frame: { srcdoc: "" },
  };
}

describe("cidTokensInHtml", () => {
  it("collects referenced cid tokens (normalized)", () => {
    const html = '<img src="cid:logo@x"><img src=cid:other@y>'; // prettier-ignore
    const tokens = cidTokensInHtml(html);
    expect(tokens.has("logo@x")).toBe(true);
    expect(tokens.has("other@y")).toBe(true);
  });
  it("handles angle brackets and url() refs", () => {
    const html = 'background:url(cid:bg@z) <img src="cid:<weird@w>">';
    const tokens = cidTokensInHtml(html);
    expect(tokens.has("bg@z")).toBe(true);
    expect(tokens.has("weird@w")).toBe(true);
  });
  it("returns an empty set when nothing references cid:", () => {
    expect(cidTokensInHtml("<p>attached images should be displayed inline</p>").size).toBe(0);
  });
  it("does not count http image refs", () => {
    const tokens = cidTokensInHtml('<img src="https://x/y.png">');
    expect(tokens.size).toBe(0);
  });
});

describe("rewriteCidImages", () => {
  it("replaces double-quoted src cid references with data URLs", () => {
    const map = new Map([["logo@x", "data:image/png;base64,AAA"]]);
    expect(rewriteCidImages('<img src="cid:logo@x">', map)).toBe(
      '<img src="data:image/png;base64,AAA">'
    );
  });
  it("handles single-quoted and unquoted src", () => {
    const map = new Map([["a@x", "data:image/png;base64,AAA"]]);
    expect(rewriteCidImages("<img src='cid:a@x'>", map)).toBe(
      '<img src="data:image/png;base64,AAA">'
    );
    expect(rewriteCidImages("src=cid:a@x", map)).toBe(
      'src="data:image/png;base64,AAA"'
    );
  });
  it("leaves references without matching data untouched", () => {
    const html = '<img src="cid:nope@x">';
    expect(rewriteCidImages(html, new Map())).toBe(html);
  });
  it("rewrites url(cid:...) in inline styles", () => {
    const map = new Map([["bg@x", "data:image/gif;base64,BBB"]]);
    expect(rewriteCidImages("background:url(cid:bg@x)", map)).toBe(
      'background:url("data:image/gif;base64,BBB")'
    );
  });
  it("matches cid tokens with angle brackets", () => {
    const map = new Map([["a@x", "data:image/png;base64,AAA"]]);
    expect(rewriteCidImages('<img src="cid:<a@x>">', map)).toBe(
      '<img src="data:image/png;base64,AAA">'
    );
  });
});

describe("renderViewSubject", () => {
  it("shows the subject", () => {
    const view = mockView();
    renderViewSubject(view, { subject: "Hello" });
    expect(view.subject.textContent).toBe("Hello");
  });
  it("falls back to (no subject)", () => {
    const view = mockView();
    renderViewSubject(view, {});
    expect(view.subject.textContent).toBe("(no subject)");
  });
});

describe("renderViewMeta", () => {
  it("shows sender and date", () => {
    const view = mockView();
    const d = new Date("2026-01-02T10:30:00Z").toLocaleString();
    renderViewMeta(view, { from: "A <a@b.c>", date: "2026-01-02T10:30:00Z" });
    expect(view.meta.textContent).toContain("A <a@b.c>");
    expect(view.meta.textContent).toContain(d);
    expect(view.meta.textContent).toContain("·");
  });
  it("handles missing sender or date", () => {
    const view = mockView();
    renderViewMeta(view, {});
    expect(view.meta.textContent).toBe("");
    renderViewMeta(view, { from: "X" });
    expect(view.meta.textContent).toBe("X");
  });
});

describe("renderViewBody", () => {
  it("renders HTML when present, sandboxed with CSP", async () => {
    const view = mockView();
    await renderViewBody(view, { html: "<html><head></head><body>hi</body></html>" });
    expect(view.frame.srcdoc).toContain('Content-Security-Policy');
    expect(view.frame.srcdoc).toContain("hi");
  });
  it("escapes plain-text bodies into a <pre>", async () => {
    const view = mockView();
    await renderViewBody(view, { text: "<script>alert(1)</script>" });
    // The email's script is escaped (inert); the only real <script> in the
    // document is the app's own link handler.
    expect(view.frame.srcdoc).toContain("&lt;script&gt;alert(1)&lt;/script&gt;");
    expect(view.frame.srcdoc).not.toContain(">alert(1)<");
  });
  it("shows an empty-message placeholder", async () => {
    const view = mockView();
    await renderViewBody(view, {});
    expect(view.frame.srcdoc).toContain("(empty message)");
  });
});
