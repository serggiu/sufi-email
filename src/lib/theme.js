// Omarchy theme integration: map the active theme's palette (the colors.toml
// the Omarchy shell stages at ~/.local/state/omarchy/current/theme/) onto the
// app's CSS custom properties. Pure and unit-tested; the Rust backend reads
// and parses colors.toml and hands this module the resulting object.
//
// The mapping follows the reading-surface convention of the built-in dark
// theme: surfaces step up from the theme background, text steps down from the
// theme foreground, the accent drives highlights, and the border sits between
// background and foreground.

// Parse "#rrggbb" (also accepts "#rgb") into [r, g, b] 0..255.
export function hexToRgb(hex) {
  let h = String(hex || "").trim().replace(/^#/, "");
  if (h.length === 3) {
    h = h.split("").map((c) => c + c).join("");
  }
  const m = /^([0-9a-f]{6})$/i.exec(h);
  if (!m) return null;
  return [
    parseInt(m[1].slice(0, 2), 16),
    parseInt(m[1].slice(2, 4), 16),
    parseInt(m[1].slice(4, 6), 16),
  ];
}

// Mix two hex colors, `amount` of `b` (0..1; "NN%" is also accepted).
// Returns a "#rrggbb" string.
export function mixHex(a, b, amount) {
  const ra = hexToRgb(a);
  const rb = hexToRgb(b);
  if (!ra || !rb) return a;
  let t = Number(amount);
  if (String(amount).endsWith("%")) t = t / 100;
  if (!Number.isFinite(t)) t = 0.5;
  t = Math.max(0, Math.min(1, t));
  const ch = (x, y) => Math.round(x + (y - x) * t);
  const [r, g, bl] = [ch(ra[0], rb[0]), ch(ra[1], rb[1]), ch(ra[2], rb[2])];
  return "#" + [r, g, bl].map((v) => v.toString(16).padStart(2, "0")).join("");
}

// The CSS variables the theme drives. Applying/removing exactly these keeps
// the mapping and the reset in sync with the built-in :root defaults.
export const THEME_VAR_NAMES = [
  "--bg",
  "--bg-alt",
  "--bg-hover",
  "--bg-active",
  "--text",
  "--text-dim",
  "--accent",
  "--border",
  "--unread",
  "--danger",
  "--warn",
];

// Apply a CSS-variable map to a root element (the document root in the app);
// null/empty resets the themed variables so the built-in defaults take over.
export function applyThemeVars(root, vars) {
  if (!vars) {
    for (const key of THEME_VAR_NAMES) {
      root.style.removeProperty(key);
    }
    return;
  }
  for (const [key, value] of Object.entries(vars)) {
    root.style.setProperty(key, value);
  }
}

// Map the theme palette onto the app's CSS variables. `colors` is the parsed
// colors.toml: { mode, background, lighter_background, selection, foreground,
// accent, red, orange, ... }. Every field the mapping needs has a fallback,
// so a minimal or slightly-off palette still produces a coherent look.
export function omarchyThemeToCssVars(colors) {
  if (!colors || typeof colors !== "object") return null;
  const bg = colors.background;
  const fg = colors.foreground;
  const accent = colors.accent;
  if (!bg || !fg || !accent) return null;
  const lighter = colors.lighter_background || bg;
  return {
    "--bg": bg,
    "--bg-alt": mixHex(bg, lighter, 0.45),
    "--bg-hover": mixHex(bg, lighter, 0.7),
    "--bg-active": colors.selection || mixHex(bg, accent, 0.25),
    "--text": mixHex(fg, bg, 0.1),
    "--text-dim": mixHex(fg, bg, 0.35),
    "--accent": accent,
    "--border": mixHex(bg, fg, 0.15),
    "--unread": fg,
    "--danger": colors.red || "#f7768e",
    "--warn": colors.orange || colors.yellow || "#f0a35e",
  };
}
