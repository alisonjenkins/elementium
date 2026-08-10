import { defineConfig } from "vitest/config";

/**
 * Unit tests for the parts of the shim that are pure functions.
 *
 * Separate from `playwright.config.ts`, which drives a real Element Web against a real
 * homeserver: that suite is minutes long and needs the stack up, so it is the wrong place
 * for a protobuf field-number table. `tests/` is Playwright's; these live beside the code.
 */
export default defineConfig({
  test: {
    // `tests/` is Playwright's, with one exception: the tone analyser the call tests judge
    // audio with is pure arithmetic, and an instrument that is only ever exercised by a
    // twelve-minute call is an instrument nobody has calibrated. Only `*.test.ts` matches,
    // so the specs themselves are still Playwright's alone.
    include: ["src/**/*.test.ts", "tests/**/*.test.ts"],
    environment: "node",
  },
});
