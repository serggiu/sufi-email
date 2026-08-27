import { defineConfig } from "vite";

// Tauri expects a fixed port; CI/dev defaults are fine for local use.
export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      ignored: ["**/src-tauri/**"],
    },
  },
  envPrefix: ["VITE_", "TAURI_"],
  build: {
    target: "chrome105",
    minify: false,
    sourcemap: true,
  },
});
