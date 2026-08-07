import { defineConfig } from "vite";
import path from "path";

/**
 * Builds the autojoin driver as its own IIFE bundle.
 *
 * Separate from the shims on purpose: this is test scaffolding that drives the application
 * into a call, and it must not be possible for it to end up in a release build by sharing an
 * entry point with product code. `patch-element-web.sh` injects it only when
 * `ELEMENTIUM_AUTOJOIN=1`.
 */
export default defineConfig({
  build: {
    lib: {
      entry: path.resolve(__dirname, "src/autojoin/index.ts"),
      name: "ElementiumAutoJoin",
      formats: ["iife"],
      fileName: () => "elementium-autojoin.js",
    },
    outDir: "dist-shims",
    emptyOutDir: false,
    minify: "esbuild",
    sourcemap: false,
    rollupOptions: { output: { inlineDynamicImports: true } },
  },
});
