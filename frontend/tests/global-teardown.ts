/**
 * Stop the MatrixRTC stack, but only if this run started it.
 *
 * The marker file is written by `global-setup` when it brought the stack up. Without
 * that check a test run would tear down an environment a developer had started for
 * something else -- which is the kind of surprise that gets test suites disabled.
 *
 * Synapse's database survives in the bind mount by design: it is the slowest part to
 * rebuild, provisioning is idempotent, and nothing in it is worth protecting. Set
 * `ELEMENTIUM_WIPE_TEST_ENV=1` to remove it too.
 */
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { rm, access } from "node:fs/promises";
import { OWNED_MARKER, TEST_ENV } from "./global-setup";

const run = promisify(execFile);

async function exists(p: string): Promise<boolean> {
  try {
    await access(p);
    return true;
  } catch {
    return false;
  }
}

export default async function globalTeardown(): Promise<void> {
  if (!(await exists(OWNED_MARKER))) {
    console.log("[test-env] stack was already running before this run; leaving it up");
    return;
  }

  console.log("[test-env] stopping the stack...");
  await run("docker", ["compose", "down"], { cwd: TEST_ENV });
  await rm(OWNED_MARKER, { force: true });

  if (process.env["ELEMENTIUM_WIPE_TEST_ENV"] === "1") {
    // Through docker, because synapse writes its database as its own uid and the
    // files are not ours to delete from outside the container.
    await run("docker", [
      "run", "--rm", "-v", `${TEST_ENV}/synapse:/data`,
      "--entrypoint", "sh", "matrixdotorg/synapse:latest",
      "-c", "rm -rf /data/homeserver.db* /data/media_store",
    ]);
    console.log("[test-env] homeserver database wiped");
  }
  console.log("[test-env] stack stopped");
}
