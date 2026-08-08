# Quickstart: validating the Element Web upgrade

**Created**: 2026-08-08

How to prove the upgrade worked, in the order that makes a failure interpretable. Each step
narrows what a later failure can mean, so running them out of order costs debugging time.

## Prerequisites

- The dev shell: everything below assumes `nix develop -c`.
- The local MatrixRTC stack (`test-env/`) for anything involving a call. The Playwright
  global setup brings it up and leaves it alone if it was already running.
- Speakers muted if you would rather not hear a 440Hz tone for three minutes.

## 1. Baseline, before moving anything

The point of this step is to have something to compare against. Run it on the version you
are already on.

```bash
nix develop -c pnpm --dir frontend exec playwright test tests/matrixrtc/shim-contract.spec.ts
```

**Expect**: all shims report `installed: true`, in both the main window and the Element Call
widget frame. See `contracts/shim-install.md`.

If this fails *before* the bump, the upgrade is not the cause and the rest of this guide
will mislead you.

## 2. Confirm the check can fail

A test that has never failed is not known to be able to. Once, by hand:

```bash
# Remove the injected script tag from element-web-dist/index.html, then:
nix develop -c pnpm --dir frontend exec playwright test tests/matrixrtc/shim-contract.spec.ts
```

**Expect**: a failure naming the document the shim map was missing from — not a timeout,
and not a generic assertion. Then re-run `scripts/patch-element-web.sh` to restore it.

## 3. Move the pin

```bash
# elementium.config.sh: ELEMENT_WEB_VERSION := v1.12.25
nix develop -c just element-web-sync v1.12.25
```

**Expect**: the report in `contracts/cli.md`. On failure the pin is left where it was, so
you are never halfway.

## 4. Prove the media path, not the page load

The bar here is feature 003's, not "the app starts".

```bash
# Terminal A — two real Element Web participants, holding a call open:
nix develop -c just call-peers

# Terminal B — Elementium joins the same call:
ELEMENTIUM_AUTOJOIN_VIDEO=0 nix develop -c just app-join
```

**Expect**, from `/tmp/elementium.log` and terminal A:

| Measure | Expected |
|---|---|
| Outbound audio | `captured_frames == sent_frames`, `dropped_channel_closed: 0`, `dropped_channel_full: 0` |
| Inbound audio | Several hundred `Decoded inbound Opus audio frame` events with a non-zero `peak_amplitude` |
| Remote video | One `Remote track: kind=video` per publishing peer |
| E2EE | Zero decrypt failures; `E2EE key decrypted its first frame` with `waited_ms` well under 5000 |
| The peers | `hears 2/2 streams` for the whole call |

`ELEMENTIUM_AUTOJOIN_VIDEO=0` keeps the webcam shut. Element Call acquires devices in its
lobby before any control exists, so this is the only way to decline it.

Stop the peers with `pkill -f "peers[.]spec"` — with the brackets, or it matches its own
shell.

## 5. The harness runs upstream too

```bash
nix develop -c pnpm --dir frontend exec playwright test
nix develop -c cargo test --workspace
```

The Playwright participants are real Element Web from `element-web-dist`, so an upstream
change can break the *harness* as easily as the product. A failure here is not automatically
a product regression.

## 6. Only if a patch is being carried

```bash
nix develop -c just element-web-rebase v1.12.25
```

**Expect**: one line per carried commit, `applied` / `dropped` / `conflicted`. A `dropped`
line means upstream took that change — see research R3, and expect a `conflicted` line
instead if a reviewer amended it on the way in.

## What "done" looks like

Steps 1–5 pass, and step 4's numbers are the ones above rather than merely non-zero. A call
that connects and carries no audio satisfies "it works" and none of this.
