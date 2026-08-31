import { describe, it, expect } from "vitest";
import {
  renderViewSubject,
  renderViewMeta,
  renderViewBody,
  rewriteCidImages,
  cidTokensInHtml,
  planAttachmentDisplay,
  cidDataMap,
  hasRemoteImages,
  placeholderRemoteImages,
  frameDisplayHeight,
  frameAvailableHeight,
  frameContentHeight,
  EMAIL_FRAME_SANDBOX,
} from "./mailview.js";
import { withEmailCsp } from "./format.js";

function mockView() {
  return {
    subject: { textContent: "" },
    meta: { textContent: "" },
    frame: { srcdoc: "" },
  };
}

describe("planAttachmentDisplay", () => {
  const att = (part_id, contentType, contentId, size) => ({
    part_id,
    contentType,
    contentId,
    size,
  });

  it("embeds parts the body references via cid: and gives them no thumbnail", () => {
    const html = '<img src="cid:logo@x">';
    const atts = [att("0", "image/png", "logo@x", 100)];
    const plan = planAttachmentDisplay(html, atts);
    expect(plan.cidParts).toEqual(atts);
    expect(plan.thumbParts).toEqual([]);
    expect(plan.wantParts).toEqual(atts);
  });

  it("keeps unreferenced parts with a Content-ID as thumbnails (regression)", () => {
    const html = "<p>attached images should be displayed inline</p>";
    const atts = [att("0", "image/jpeg", "f_mtee1cqi1", 265759)];
    const plan = planAttachmentDisplay(html, atts);
    expect(plan.cidParts).toEqual([]);
    expect(plan.thumbParts).toEqual(atts);
    expect(plan.wantParts).toEqual(atts);
  });

  it("mixes embedded, unreferenced, and plain attachments", () => {
    const html = '<img src="cid:a@x">';
    const embedded = att("0", "image/png", "a@x", 100);
    const unreferenced = att("1", "image/jpeg", "b@x", 200);
    const plain = att("2", "image/gif", null, 300);
    const plan = planAttachmentDisplay(html, [embedded, unreferenced, plain]);
    expect(plan.cidParts).toEqual([embedded]);
    expect(plan.thumbParts).toEqual([unreferenced, plain]);
    // wantParts: embedded parts first, then thumbnails, deduplicated.
    expect(plan.wantParts).toEqual([embedded, unreferenced, plain]);
  });

  it("matches content-id tokens with angle brackets", () => {
    const html = '<img src="cid:<logo@x>">';
    const atts = [att("0", "image/png", "logo@x", 100)];
    expect(planAttachmentDisplay(html, atts).cidParts).toEqual(atts);
  });

  it("excludes non-image and oversized parts from previews", () => {
    const pdf = att("0", "application/pdf", null, 100);
    const big = att("1", "image/png", null, 10 * 1024 * 1024); // > 3MB preview cap
    const small = att("2", "image/png", null, 1024);
    const plan = planAttachmentDisplay("<p>x</p>", [pdf, big, small]);
    expect(plan.cidParts).toEqual([]);
    expect(plan.thumbParts).toEqual([small]);
    expect(plan.wantParts).toEqual([small]);
  });

  it("honors custom size limits", () => {
    const html = '<img src="cid:big@x">';
    const bigCid = att("0", "image/png", "big@x", 9 * 1024 * 1024); // > 8MB default cid cap
    expect(planAttachmentDisplay(html, [bigCid]).cidParts).toEqual([]);
    expect(planAttachmentDisplay(html, [bigCid], { maxCidBytes: 10 * 1024 * 1024 }).cidParts).toEqual([
      bigCid,
    ]);
  });

  it("returns empty plans for no body and empty attachments", () => {
    expect(planAttachmentDisplay(null, [])).toEqual({
      cidParts: [],
      thumbParts: [],
      wantParts: [],
    });
  });
});

describe("cidDataMap", () => {
  it("builds data: URLs for embedded parts with fetched data", () => {
    const parts = [{ part_id: "0", contentType: "image/png", contentId: "a@x" }];
    const map = cidDataMap(parts, new Map([["0", "QUJD"]]));
    expect(map.get("a@x")).toBe("data:image/png;base64,QUJD");
  });
  it("lowercases map keys so header/reference case mismatches still match", () => {
    const parts = [{ part_id: "0", contentType: "image/png", contentId: "Logo@X" }];
    const map = cidDataMap(parts, new Map([["0", "QUJD"]]));
    // The key is stored lowercased; rewriteCidImages lowercases the
    // reference before looking up, so `cid:logo@x` still resolves.
    expect(map.get("logo@x")).toBe("data:image/png;base64,QUJD");
    expect(map.has("Logo@X")).toBe(false);
  });
  it("skips parts without fetched data", () => {
    const parts = [{ part_id: "0", contentType: "image/png", contentId: "a@x" }];
    expect(cidDataMap(parts, new Map()).size).toBe(0);
  });
});

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
  it("collects cid tokens from srcset candidates", () => {
    const tokens = cidTokensInHtml('<img srcset="cid:a@x 1x, cid:b@y 2x">');
    expect(tokens.has("a@x")).toBe(true);
    expect(tokens.has("b@y")).toBe(true);
  });
  it("collects cid tokens from svg image href", () => {
    const html = '<svg><image href="cid:svg@x"></image></svg>';
    const tokens = cidTokensInHtml(html);
    expect(tokens.has("svg@x")).toBe(true);
  });
  it("does not collect cid tokens from ordinary link hrefs", () => {
    expect(cidTokensInHtml('<a href="cid:mailto@x">x</a>').size).toBe(0);
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
  it("turns a cid: srcset into a single data: src (data URLs break srcset grammar)", () => {
    const map = new Map([["a@x", "data:image/png;base64,AAA"]]);
    expect(rewriteCidImages('<img srcset="cid:a@x 1x, cid:b@y 2x">', map)).toBe(
      '<img src="data:image/png;base64,AAA">'
    );
  });
  it("drops a cid: srcset that has no matching part, falling back to the tag's src", () => {
    const map = new Map([["a@x", "data:image/png;base64,AAA"]]);
    // srcset references an unknown cid; the (cid) src is rewritten normally.
    expect(rewriteCidImages('<img src="cid:a@x" srcset="cid:nope@x 1x">', map)).toBe(
      '<img src="data:image/png;base64,AAA">'
    );
  });
  it("rewrites svg image href / xlink:href cid references", () => {
    const map = new Map([["svg@x", "data:image/svg+xml;base64,QQ"]]);
    const html = '<svg><image href="cid:svg@x"></image></svg>';
    expect(rewriteCidImages(html, map)).toBe(
      '<svg><image href="data:image/svg+xml;base64,QQ"></image></svg>'
    );
  });
  it("matches cid references case-insensitively against the content-id", () => {
    const map = new Map([["logo@x", "data:image/png;base64,AAA"]]);
    expect(rewriteCidImages('<img src="cid:Logo@X">', map)).toBe(
      '<img src="data:image/png;base64,AAA">'
    );
  });
  it("leaves remote srcset values untouched", () => {
    const html = '<img src="cid:a@x" srcset="https://x/y.png 2x">';
    const map = new Map([["a@x", "data:image/png;base64,AAA"]]);
    expect(rewriteCidImages(html, map)).toBe(
      '<img src="data:image/png;base64,AAA" srcset="https://x/y.png 2x">'
    );
  });
  it("preserves non-cid attributes when rewriting media tags", () => {
    const map = new Map([["a@x", "data:image/png;base64,AAA"]]);
    const out = rewriteCidImages('<img src="cid:a@x" width="200" alt="hi">', map);
    expect(out).toBe('<img src="data:image/png;base64,AAA" width="200" alt="hi">');
  });
});

describe("hasRemoteImages", () => {
  it("detects https/http src images", () => {
    expect(hasRemoteImages('<img src="https://x/y.png">')).toBe(true);
    expect(hasRemoteImages('<img src="http://x/y.png">')).toBe(true);
  });
  it("detects protocol-relative and srcset references", () => {
    expect(hasRemoteImages('<img src="//cdn.x/y.png">')).toBe(true);
    expect(hasRemoteImages('<img srcset="//cdn.x/a.png 1x, /b.png 2x">')).toBe(true);
  });
  it("detects css url() and background/poster attributes", () => {
    expect(hasRemoteImages('<div style="background:url(https://x/bg.png)">x</div>')).toBe(true);
    expect(hasRemoteImages('<body background="https://x/bg.png">')).toBe(true);
    expect(hasRemoteImages('<video poster="https://x/p.png"></video>')).toBe(true);
  });
  it("detects svg image href", () => {
    expect(hasRemoteImages('<svg><image href="https://x/i.png"></image></svg>')).toBe(true);
  });
  it("is false for cid:, data:, and empty bodies", () => {
    expect(hasRemoteImages('<img src="cid:a@x">')).toBe(false);
    expect(hasRemoteImages('<img src="data:image/png;base64,AAA">')).toBe(false);
    expect(hasRemoteImages("<p>no images here</p>")).toBe(false);
    expect(hasRemoteImages(null)).toBe(false);
  });
});

describe("placeholderRemoteImages", () => {
  const BLANK = "data:image/gif;base64,R0lGODlhAQABAAAAACH5BAEKAAEALAAAAAABAAEAAAICTAEAOw==";

  it("replaces remote src references with a blank gif", () => {
    expect(placeholderRemoteImages('<img src="https://x/y.png">')).toBe(
      `<img src="${BLANK}">`
    );
  });
  it("leaves cid: and data: references untouched", () => {
    expect(placeholderRemoteImages('<img src="cid:a@x" src="data:image/png;base64,AAA">')).toBe(
      '<img src="cid:a@x" src="data:image/png;base64,AAA">'
    );
  });
  it("replaces remote css url() and drops remote srcset candidates", () => {
    expect(placeholderRemoteImages('<div style="background:url(https://x/bg.png)">x</div>')).toBe(
      `<div style="background:url(\"${BLANK}\")">x</div>`
    );
    // A remote srcset is dropped (a data: URL would break srcset's comma
    // grammar); the tag falls back to its own (blanked) src.
    expect(placeholderRemoteImages('<img src="https://x/a.png" srcset="https://x/a.png 1x, /b.png 2x">')).toBe(
      `<img src="${BLANK}">`
    );
    expect(placeholderRemoteImages('<img srcset="https://x/a.png 1x">')).toBe("<img>");
  });
  it("replaces remote svg image href", () => {
    expect(placeholderRemoteImages('<svg><image href="https://x/i.png"></image></svg>')).toBe(
      `<svg><image href="${BLANK}"></image></svg>`
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
    // The email's script is escaped to inert text — no real <script>
    // element from the message exists (the sender's markup is escaped; only
    // the injected nonce'd link handler is a genuine script, and the email
    // cannot run its own code because it lacks the nonce).
    expect(view.frame.srcdoc).toContain("&lt;script&gt;alert(1)&lt;/script&gt;");
    expect(view.frame.srcdoc).not.toContain(">alert(1)<");
    // The only <script> in the document is the injected nonce'd handler.
    expect(view.frame.srcdoc).not.toContain("<script>alert(1)");
    expect(view.frame.srcdoc).toContain('<script nonce="');
  });
  it("shows an empty-message placeholder", async () => {
    const view = mockView();
    await renderViewBody(view, {});
    expect(view.frame.srcdoc).toContain("(empty message)");
  });
  it("passes the remote-images option through to the injected CSP", async () => {
    const view = mockView();
    await renderViewBody(
      view,
      { html: "<html><head></head><body>hi</body></html>" },
      { remoteImages: true }
    );
    expect(view.frame.srcdoc).toContain("img-src * data: blob:");
    // Default (no option): strict policy, remote images still blocked.
    const strict = mockView();
    await renderViewBody(strict, { html: "<html><head></head><body>hi</body></html>" });
    expect(strict.frame.srcdoc).toContain("img-src data:");
    expect(strict.frame.srcdoc).not.toContain("img-src *");
  });
});

describe("frameDisplayHeight", () => {
  it("fills the entire available height for content shorter than the pane", () => {
    // A short message must use the whole reading area, not sit in a
    // fixed-height box at the top.
    expect(frameDisplayHeight(200, 800)).toBe(800);
  });

  it("keeps the full content height when it exceeds the pane (no nested scrollbar)", () => {
    // Regression: the iframe must never be shorter than its content, or a
    // scrollbar appears INSIDE the message body and the outer container
    // scrolls too — the "fixed height window" the user reported.
    expect(frameDisplayHeight(5000, 800)).toBe(5000);
    expect(frameDisplayHeight(900, 800)).toBe(900);
  });

  it("matches the content height when content and available space agree", () => {
    expect(frameDisplayHeight(800, 800)).toBe(800);
  });

  it("never falls below a small floor for empty content", () => {
    expect(frameDisplayHeight(0, 0)).toBe(80);
    expect(frameDisplayHeight(0, 600)).toBe(600);
  });

  it("uses the content height when the container reports no space yet", () => {
    // The modal is opened before its thread renders, so frames can report
    // while #modal-thread's client height is still 0 — the content height
    // must win, not collapse to the floor.
    expect(frameDisplayHeight(250, 0)).toBe(250);
  });
});

describe("frameAvailableHeight", () => {
  it("reads the height from the frame's scroll container", () => {
    const frame = { closest: () => ({ clientHeight: 640 }) };
    expect(frameAvailableHeight(frame)).toBe(640);
  });

  it("falls back to the window height when no container matches", () => {
    const frame = { closest: () => null };
    expect(frameAvailableHeight(frame)).toBe(window.innerHeight);
  });
});

describe("email frame sandbox + parent-side measurement (reading-pane fix)", () => {
  it("sizes the iframe from the parent via a same-origin sandbox", () => {
    // Regression: the reading pane must fill the available height. The
    // parent measures the message document directly, which requires the
    // sandbox to grant allow-same-origin (the old in-iframe script never
    // ran in the release webview, leaving messages in a fixed ~150px box).
    expect(EMAIL_FRAME_SANDBOX).toContain("allow-same-origin");
  });

  it("keeps email scripts blocked by the injected CSP", () => {
    // The sandbox includes allow-scripts — it is what lets the injected
    // nonce'd link handler run inside the email (the only way WebKitGTK
    // delivers real clicks to a handler) — but the email's own scripts must
    // stay dead: the injected CSP allows exactly one script, pinned by a
    // random nonce the email cannot know, so the untrusted message cannot
    // execute its own code even though it shares the origin (which the
    // parent needs to measure the frame).
    expect(EMAIL_FRAME_SANDBOX).toContain("allow-same-origin");
    expect(EMAIL_FRAME_SANDBOX).toContain("allow-scripts");
    expect(withEmailCsp("<html><head></head><body>hi</body></html>")).toMatch(
      /script-src 'nonce-[0-9a-f]+'/
    );
    expect(withEmailCsp("<html><head></head><body>hi</body></html>")).not.toContain(
      "script-src 'none'"
    );
  });

  it("measures the message height from the taller of body/documentElement", () => {
    const doc = {
      body: { scrollHeight: 3639 },
      documentElement: { scrollHeight: 3673 },
    };
    expect(frameContentHeight(doc)).toBe(3673);
    expect(
      frameContentHeight({ body: { scrollHeight: 500 }, documentElement: { scrollHeight: 400 } })
    ).toBe(500);
  });

  it("reports 0 for an unreadable/empty document", () => {
    expect(frameContentHeight(null)).toBe(0);
    expect(frameContentHeight({})).toBe(0);
  });

  it("combines measurement + sizing: long content keeps full height, short fills the pane", () => {
    const longDoc = { body: { scrollHeight: 3673 }, documentElement: { scrollHeight: 3673 } };
    const shortDoc = { body: { scrollHeight: 150 }, documentElement: { scrollHeight: 150 } };
    const available = 758;
    // Tall message: the iframe keeps its full content height and the OUTER
    // container scrolls — never a nested scrollbar inside the message.
    expect(frameDisplayHeight(frameContentHeight(longDoc), available)).toBe(3673);
    // Short message: fills the entire available reading height.
    expect(frameDisplayHeight(frameContentHeight(shortDoc), available)).toBe(available);
  });
});
