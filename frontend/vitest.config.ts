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
    include: ["src/**/*.test.ts"],
    environment: "node",
  },
});
