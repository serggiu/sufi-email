import { describe, it, expect } from "vitest";
import {
  hexToRgb,
  mixHex,
  omarchyThemeToCssVars,
  applyThemeVars,
  THEME_VAR_NAMES,
} from "./theme.js";

describe("hexToRgb", () => {
  it("parses #rrggbb", () => {
    expect(hexToRgb("#2e3440")).toEqual([46, 52, 64]);
    expect(hexToRgb("d8dee9")).toEqual([216, 222, 233]);
  });
  it("expands #rgb shorthand", () => {
    expect(hexToRgb("#abc")).toEqual([170, 187, 204]);
  });
  it("returns null for garbage", () => {
    expect(hexToRgb("nope")).toBeNull();
    expect(hexToRgb("")).toBeNull();
  });
});

describe("mixHex", () => {
  it("mixes two colors by a fraction", () => {
    expect(mixHex("#000000", "#ffffff", 0.5)).toBe("#808080");
    expect(mixHex("#ff0000", "#0000ff", 0.5)).toBe("#800080");
  });
  it("accepts percentage amounts", () => {
    expect(mixHex("#000000", "#ffffff", "50%")).toBe("#808080");
  });
  it("clamps and falls back", () => {
    expect(mixHex("#000000", "#ffffff", 2)).toBe("#ffffff");
    expect(mixHex("#ffffff", "#000000", -1)).toBe("#ffffff");
    expect(mixHex("#000000", "#ffffff", "oops")).toBe("#808080");
  });
});

describe("omarchyThemeToCssVars", () => {
  // Nord, the palette the tests compare against.
  const nord = {
    mode: "dark",
    accent: "#81a1c1",
    selection: "#434c5e",
    muted: "#4c566a",
    background: "#2e3440",
    lighter_background: "#3b4252",
    foreground: "#d8dee9",
    red: "#bf616a",
    orange: "#d5967a",
  };

  it("maps the Nord palette onto the app variables", () => {
    const vars = omarchyThemeToCssVars(nord);
    expect(vars["--bg"]).toBe("#2e3440");
    expect(vars["--text"]).toBe("#c7cdd8"); // foreground dimmed 10% toward bg
    expect(vars["--text-dim"]).toBe("#9da3ae"); // foreground dimmed 35% toward bg
    expect(vars["--accent"]).toBe("#81a1c1");
    expect(vars["--unread"]).toBe("#d8dee9");
    expect(vars["--bg-active"]).toBe("#434c5e"); // selection
    expect(vars["--danger"]).toBe("#bf616a");
    expect(vars["--warn"]).toBe("#d5967a");
    // surface steps sit between background and lighter_background
    const bg = hexToRgb(vars["--bg"]);
    const alt = hexToRgb(vars["--bg-alt"]);
    const hover = hexToRgb(vars["--bg-hover"]);
    expect(alt[0]).toBeGreaterThan(bg[0]);
    expect(hover[0]).toBeGreaterThan(alt[0]);
    // border sits between background and foreground
    expect(hexToRgb(vars["--border"])[0]).toBeGreaterThan(bg[0]);
  });

  it("works for a light theme palette too", () => {
    const light = {
      mode: "light",
      accent: "#356fc7",
      selection: "#d3e0f5",
      background: "#ffffff",
      lighter_background: "#f2f4f8",
      foreground: "#1c1e21",
      red: "#c92a2a",
      orange: "#e8590c",
    };
    const vars = omarchyThemeToCssVars(light);
    expect(vars["--bg"]).toBe("#ffffff");
    expect(vars["--text"]).toBe("#333537"); // dark text, 10% lightened toward bg
    expect(vars["--accent"]).toBe("#356fc7");
  });

  it("falls back when a palette field is missing", () => {
    const minimal = {
      mode: "dark",
      background: "#111111",
      foreground: "#eeeeee",
      accent: "#8888ff",
    };
    const vars = omarchyThemeToCssVars(minimal);
    expect(vars["--bg-alt"]).toBeTruthy(); // uses background as lighter fallback
    expect(vars["--bg-active"]).toBeTruthy(); // mixes accent as selection fallback
    expect(vars["--danger"]).toBe("#f7768e"); // built-in danger fallback
    expect(vars["--warn"]).toBe("#f0a35e"); // built-in warn fallback
  });

  it("returns null for a non-palette input", () => {
    expect(omarchyThemeToCssVars(null)).toBeNull();
    expect(omarchyThemeToCssVars({})).toBeNull();
    expect(omarchyThemeToCssVars({ background: "#fff", foreground: "#000" })).toBeNull();
  });
});

describe("applyThemeVars (DOM application)", () => {
  it("sets the mapped variables on the root element", () => {
    const root = document.documentElement;
    applyThemeVars(root, { "--bg": "#2e3440", "--accent": "#81a1c1" });
    expect(root.style.getPropertyValue("--bg")).toBe("#2e3440");
    expect(root.style.getPropertyValue("--accent")).toBe("#81a1c1");
  });

  it("removes every themed variable when reset (fall back to built-in defaults)", () => {
    const root = document.documentElement;
    applyThemeVars(root, { "--bg": "#2e3440", "--text": "#d8dee9" });
    applyThemeVars(root, null);
    for (const key of THEME_VAR_NAMES) {
      expect(root.style.getPropertyValue(key)).toBe("");
    }
  });
});
