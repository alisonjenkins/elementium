import { defineConfig } from "vite";

export default defineConfig({
  // Tauri expects a fixed port for dev
  server: {
    port: 5173,
    strictPort: true,
  },

  // Clear the screen in dev mode
  clearScreen: false,

  // Environment variables that start with TAURI_ are exposed to the client
  envPrefix: ["VITE_", "TAURI_"],

  build: {
    // Tauri uses WebKit on Linux which has limited ES module support
    target: ["es2021", "chrome100", "safari15"],
    // Don't minify in debug builds
    minify: !process.env.TAURI_DEBUG ? "esbuild" : false,
    // Produce sourcemaps for debug builds
    sourcemap: !!process.env.TAURI_DEBUG,
    outDir: "dist",
  },
});
