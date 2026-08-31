// Guards the app-level CSP in tauri.conf.json — the config that was
// silently breaking opted-in remote images.
//
// WebKit applies the embedding document's CSP to the email-body srcdoc
// iframes, so the email's own (relaxed) meta CSP can never win on its own:
// the app CSP must ALSO permit remote images for the "Load images" opt-in
// to work. This test pins that contract so a future tightening of the app
// CSP cannot regress the feature — while still asserting the lockdown
// that keeps email exfiltration impossible.
import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

// vitest runs from the project root (npm test), so tauri.conf.json is
// at ./src-tauri/tauri.conf.json relative to cwd.
const config = JSON.parse(readFileSync(resolve("src-tauri/tauri.conf.json"), "utf8"));

function cspDirective(csp, name) {
  const m = new RegExp(`${name}\\s+([^;]+)`).exec(csp);
  return m ? m[1].trim() : null;
}

describe("app CSP (tauri.conf.json)", () => {
  it("permits remote images for opted-in email bodies", () => {
    for (const key of ["csp", "devCsp"]) {
      const imgSrc = cspDirective(config.app.security[key], "img-src");
      // The email iframe inherits the app CSP; the opt-in only works when
      // this policy allows https/http images too.
      expect(imgSrc).toBeTruthy();
      expect(imgSrc).toMatch(/\*|https:/);
      // data: must stay allowed for embedded cid: images and placeholders.
      expect(imgSrc).toContain("data:");
    }
  });

  it("also allows remote media/fonts (the opt-in's media-src/font-src)", () => {
    const csp = config.app.security.csp;
    expect(cspDirective(csp, "media-src")).toMatch(/\*|https:/);
    expect(cspDirective(csp, "font-src")).toMatch(/\*|https:/);
  });

  it("keeps email exfiltration locked down", () => {
    const csp = config.app.security.csp;
    // No network fetch: connect-src stays limited to the local Tauri IPC
    // bridge (ipc: / http://ipc.localhost) — never a wildcard and never a
    // remote origin (https: would allow exfiltration to any server).
    const connectSrc = cspDirective(csp, "connect-src");
    expect(connectSrc).not.toMatch(/\*/);
    expect(connectSrc).not.toMatch(/https:/);
    // The only http: allowed is the localhost IPC bridge.
    expect(connectSrc).toContain("http://ipc.localhost");
    // Scripts stay same-origin only in release (no 'unsafe-inline'), so
    // the email's own inline scripts cannot run even if they tried.
    expect(csp).not.toMatch(/script-src[^;]*'unsafe-inline'/);
    // Frames/objects stay blocked (default-src 'none' inside the email).
    expect(cspDirective(csp, "default-src")).toContain("'self'");
  });

  it("blocks remote images by default at the email layer (strict meta CSP)", async () => {
    // The email's own strict policy is what blocks remote images before
    // the user opts in; keep that contract in mind alongside the app CSP.
    const { withEmailCsp } = await import("./format.js");
    const strict = withEmailCsp("<html><head></head><body>x</body></html>");
    expect(strict).toContain("img-src data:");
    expect(strict).not.toContain("img-src *");
  });
});
