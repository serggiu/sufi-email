import { describe, it, expect } from "vitest";
import { renderViewSubject, renderViewMeta, renderViewBody } from "./mailview.js";

function mockView() {
  return {
    subject: { textContent: "" },
    meta: { textContent: "" },
    frame: { srcdoc: "" },
  };
}

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
