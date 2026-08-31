import { describe, it, expect, beforeEach } from "vitest";
import { JSDOM } from "jsdom";
import {
  REMOTE_IMAGES_ALWAYS_KEY,
  remoteImagesKey,
  remoteImagesAlways,
  remoteImagesAllowed,
  rememberRemoteImages,
  rememberRemoteImagesAlways,
  buildRemoteImageBar,
} from "./remote-images.js";

// A localStorage-shaped mock; a Map-backed object that also throws when
// told to (storage failures must fall back to "blocked", never throw).
function mockStorage({ throwing = false } = {}) {
  const map = new Map();
  return {
    getItem(k) {
      if (throwing) throw new Error("denied");
      return map.has(k) ? map.get(k) : null;
    },
    setItem(k, v) {
      if (throwing) throw new Error("denied");
      map.set(k, String(v));
    },
    _map: map,
  };
}

describe("remote image consent (pure helpers)", () => {
  it("keys one message by account/folder/uid", () => {
    expect(remoteImagesKey("A", "INBOX", 42)).toBe("sufi-remote-images:A:INBOX:42");
  });

  it("is blocked by default", () => {
    const s = mockStorage();
    expect(remoteImagesAlways(s)).toBe(false);
    expect(remoteImagesAllowed(s, "A", "INBOX", 42)).toBe(false);
  });

  it("remembers per-message consent and honors it", () => {
    const s = mockStorage();
    expect(remoteImagesAllowed(s, "A", "INBOX", 42)).toBe(false);
    rememberRemoteImages(s, "A", "INBOX", 42);
    expect(s.getItem(remoteImagesKey("A", "INBOX", 42))).toBe("1");
    expect(remoteImagesAllowed(s, "A", "INBOX", 42)).toBe(true);
    // Other messages/folders stay blocked.
    expect(remoteImagesAllowed(s, "A", "INBOX", 43)).toBe(false);
    expect(remoteImagesAllowed(s, "A", "Drafts", 42)).toBe(false);
  });

  it("global always-load overrides per-message consent", () => {
    const s = mockStorage();
    rememberRemoteImagesAlways(s);
    expect(s.getItem(REMOTE_IMAGES_ALWAYS_KEY)).toBe("1");
    expect(remoteImagesAlways(s)).toBe(true);
    expect(remoteImagesAllowed(s, "A", "INBOX", 99)).toBe(true);
  });

  it("falls back to blocked when storage access fails", () => {
    const s = mockStorage({ throwing: true });
    expect(remoteImagesAlways(s)).toBe(false);
    expect(remoteImagesAllowed(s, "A", "INBOX", 42)).toBe(false);
    // And remembering quietly does nothing.
    rememberRemoteImages(s, "A", "INBOX", 42);
    rememberRemoteImagesAlways(s);
    expect(remoteImagesAllowed(s, "A", "INBOX", 42)).toBe(false);
  });
});

describe("buildRemoteImageBar", () => {
  let dom, doc, container, before;
  beforeEach(() => {
    dom = new JSDOM("<div id='block'><iframe></iframe></div>");
    doc = dom.window.document;
    container = doc.getElementById("block");
    before = doc.querySelector("iframe");
  });

  const renderSpy = () => {
    let calls = 0;
    return {
      fn: () => {
        calls++;
      },
      count: () => calls,
    };
  };

  it("inserts an explanatory banner above the body frame", () => {
    buildRemoteImageBar(container, before, 42, {
      storage: mockStorage(),
      accountName: "A",
      folder: "INBOX",
      render: () => {},
      doc,
    });
    const bar = container.querySelector(".remote-bar");
    expect(bar).not.toBeNull();
    expect(bar.nextElementSibling).toBe(before); // sits right above the frame
    expect(bar.textContent).toContain("Remote images are blocked");
    const btns = bar.querySelectorAll("button");
    expect(btns.length).toBe(2);
    expect(btns[0].textContent).toBe("Load images");
    expect(btns[1].textContent).toBe("Always load images");
  });

  it("'Load images' remembers this message and re-renders with images", () => {
    const s = mockStorage();
    const spy = renderSpy();
    buildRemoteImageBar(container, before, 42, {
      storage: s,
      accountName: "A",
      folder: "INBOX",
      render: spy.fn,
      doc,
    });
    container.querySelector(".remote-bar-btn").click();
    expect(s.getItem(remoteImagesKey("A", "INBOX", 42))).toBe("1");
    expect(spy.count()).toBe(1);
    // The banner is gone after the choice.
    expect(container.querySelector(".remote-bar")).toBeNull();
  });

  it("'Always load images' sets the global flag and re-renders", () => {
    const s = mockStorage();
    const spy = renderSpy();
    buildRemoteImageBar(container, before, 7, {
      storage: s,
      accountName: "A",
      folder: "INBOX",
      render: spy.fn,
      doc,
    });
    const buttons = container.querySelectorAll(".remote-bar-btn");
    buttons[1].click(); // Always load images
    expect(s.getItem(REMOTE_IMAGES_ALWAYS_KEY)).toBe("1");
    expect(remoteImagesAlways(s)).toBe(true);
    expect(spy.count()).toBe(1);
    expect(container.querySelector(".remote-bar")).toBeNull();
  });

  it("does not record consent for other messages on 'Load images'", () => {
    const s = mockStorage();
    buildRemoteImageBar(container, before, 42, {
      storage: s,
      accountName: "A",
      folder: "INBOX",
      render: () => {},
      doc,
    });
    container.querySelector(".remote-bar-btn").click();
    expect(remoteImagesAllowed(s, "A", "INBOX", 43)).toBe(false);
  });
});
