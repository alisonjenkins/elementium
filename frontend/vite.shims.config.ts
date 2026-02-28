import { defineConfig } from "vite";
import path from "path";

/**
 * Vite config that builds the Elementium shims as a single IIFE bundle.
 * This is injected into Element Web's index.html before its own scripts.
 */
export default defineConfig({
  build: {
    lib: {
      entry: path.resolve(__dirname, "src/shim/index.ts"),
      name: "ElementiumShims",
      formats: ["iife"],
      fileName: () => "elementium-shims.js",
    },
    outDir: "dist-shims",
    emptyOutDir: true,
    minify: "esbuild",
    sourcemap: false,
    rollupOptions: {
      output: {
        // No code splitting — single file
        inlineDynamicImports: true,
      },
    },
  },
  resolve: {
    alias: {
      "livekit-client": path.resolve(__dirname, "src/shim/livekit-bridge.ts"),
    },
  },
});
