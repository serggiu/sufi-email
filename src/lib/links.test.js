import { describe, it, expect, afterEach } from "vitest";
import {
  shouldHandleFrameLoad,
  emailLinkUrlFromMessage,
  hrefFromEmailClick,
} from "./links.js";

describe("shouldHandleFrameLoad", () => {
  it("ignores the initial about:blank document (the load-handler bug)", () => {
    // WebKitGTK fires a load event for BOTH the blank document the iframe
    // starts with and the real srcdoc content. Acting on the blank one
    // attaches handlers to a document that is immediately discarded —
    // the exact regression that made every link dead.
    expect(shouldHandleFrameLoad({ URL: "about:blank" })).toBe(false);
    expect(shouldHandleFrameLoad(null)).toBe(false);
    expect(shouldHandleFrameLoad(undefined)).toBe(false);
  });

  it("acts on the real message document and any other non-blank doc", () => {
    expect(shouldHandleFrameLoad({ URL: "about:srcdoc" })).toBe(true);
    expect(shouldHandleFrameLoad({ URL: "https://example.com/mail" })).toBe(true);
  });
});

describe("emailLinkUrlFromMessage", () => {
  it("accepts the handler's http(s) payloads", () => {
    expect(
      emailLinkUrlFromMessage({ type: "sufi-open-url", url: "https://a.example/x" })
    ).toBe("https://a.example/x");
    expect(
      emailLinkUrlFromMessage({ type: "sufi-open-url", url: "http://a.example" })
    ).toBe("http://a.example");
  });

  it("rejects non-http(s) schemes, so javascript:/mailto: never open", () => {
    expect(
      emailLinkUrlFromMessage({ type: "sufi-open-url", url: "javascript:alert(1)" })
    ).toBeNull();
    expect(
      emailLinkUrlFromMessage({ type: "sufi-open-url", url: "mailto:x@y.example" })
    ).toBeNull();
    expect(
      emailLinkUrlFromMessage({ type: "sufi-open-url", url: "file:///etc/passwd" })
    ).toBeNull();
  });

  it("rejects malformed or unrelated messages", () => {
    expect(emailLinkUrlFromMessage(null)).toBeNull();
    expect(emailLinkUrlFromMessage({ type: "other", url: "https://a.example" })).toBeNull();
    expect(emailLinkUrlFromMessage({ type: "sufi-open-url" })).toBeNull();
    expect(emailLinkUrlFromMessage({ type: "sufi-open-url", url: 42 })).toBeNull();
    expect(emailLinkUrlFromMessage({ type: "sufi-open-url", url: "" })).toBeNull();
  });
});

describe("hrefFromEmailClick", () => {
  const anchor = (href) => {
    const a = document.createElement("a");
    a.href = href;
    a.textContent = "link";
    document.body.appendChild(a);
    return a;
  };
  const click = (a, opts = {}) =>
    new MouseEvent("click", {
      bubbles: true,
      cancelable: true,
      button: 0,
      ...opts,
    });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("returns the http(s) href of a plain left-click on a link", () => {
    const a = anchor("https://example.com/x");
    expect(hrefFromEmailClick({ target: a, button: 0, defaultPrevented: false })).toBe(
      "https://example.com/x"
    );
    // The event passes through the real dispatch path too.
    const ev = click(a);
    a.dispatchEvent(ev);
    expect(hrefFromEmailClick(ev)).toBe("https://example.com/x");
  });

  it("finds a link that wraps the clicked element (closest)", () => {
    const a = anchor("https://example.com/deep");
    const inner = document.createElement("span");
    inner.textContent = "deep";
    a.appendChild(inner);
    const ev = click(inner);
    inner.dispatchEvent(ev);
    expect(hrefFromEmailClick(ev)).toBe("https://example.com/deep");
  });

  it("returns null for non-http(s) links", () => {
    const a = anchor("javascript:alert(1)");
    const ev = click(a);
    a.dispatchEvent(ev);
    expect(hrefFromEmailClick(ev)).toBeNull();
  });

  it("returns null when the click was not on a link", () => {
    const div = document.createElement("div");
    div.textContent = "not a link";
    document.body.appendChild(div);
    const ev = click(div);
    div.dispatchEvent(ev);
    expect(hrefFromEmailClick(ev)).toBeNull();
    expect(hrefFromEmailClick({ target: null, button: 0 })).toBeNull();
  });

  it("returns null for already-handled or non-left clicks", () => {
    const a = anchor("https://example.com/x");
    expect(
      hrefFromEmailClick({ target: a, button: 0, defaultPrevented: true })
    ).toBeNull();
    expect(
      hrefFromEmailClick({ target: a, button: 2, defaultPrevented: false })
    ).toBeNull();
  });
});
