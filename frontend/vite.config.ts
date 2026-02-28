import { defineConfig } from "vite";
import path from "path";

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

  resolve: {
    alias: {
      // Intercept livekit-client imports and route to our native bridge
      "livekit-client": path.resolve(__dirname, "src/shim/livekit-bridge.ts"),
    },
  },

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
