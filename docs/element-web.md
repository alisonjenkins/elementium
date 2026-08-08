# Element Web: upgrading, and where a change belongs

Elementium is Element Web with native media underneath it. Two questions come up often
enough to write down: how to move to a new upstream release, and where to put a change you
want to make.

The second is the one that decides whether this stays maintainable. Put a change in the
wrong place and it either has to be carried forever or can never be contributed.

## Where a change belongs

Three homes. Pick by asking the question in the third column — not by which is easiest.

| Kind of change | Home | The test |
|---|---|---|
| Host integration | a runtime shim in `frontend/src/shim/` | "Would this make sense only in a browser that had our native backend, and nowhere else?" |
| Product change | a commit on the Element Web patch branch | "Would I be willing to open the pull request today?" |
| Packaging | `scripts/patch-element-web.sh` | "Is this about how the artefact is assembled, rather than what it does?" |

**Host integration** is things like replacing `RTCPeerConnection` with one that forwards to
Rust, or backing `localStorage` with the system keyring. These patch *browser* globals, not
Element Web, which is why they survive upgrades largely untouched. They are also not
upstream-shaped by nature and should not be offered: "replace WebRTC with IPC to a Rust
process" is not a contribution Element Web would take.

**A product change** is anything a person would recognise as an Element Web feature or fix.
It goes on the patch branch as one atomic commit, written as if the pull request were being
opened today — because contributing it later is `git cherry-pick` and nothing else. When it
merges upstream it disappears from our branch at the next rebase automatically.

**Packaging** covers removing the CSP meta tag and injecting the shims script. It cannot be
done any other way against a prebuilt release, and carrying a patch for it would mean
maintaining a diff for something that is not a behaviour change at all.

If a change seems to want two homes, it is usually two changes.

## Upgrading

```bash
just element-web-sync v1.12.25
```

Fetches the release, rebuilds and re-injects the shims, and runs the shim contract checks in
a real browser. **The pin is written only if all of that passes.** On failure it restores the
version you were on, because a half-applied upgrade leaves nobody able to say which version
they are debugging.

It answers *do the shims install*. It does not answer *do calls work*, and the difference is
not academic — see the v1.12.25 finding below. For that:

```bash
just call-peers        # one terminal: two real Element Web participants hold a call
just app-join          # another: Elementium joins it
```

`specs/007-element-web-upgrade/quickstart.md` lists the numbers to expect. The bar is frames
sent and received, not "the app started".

## When something fails

**A patch step fails.** The message names the step and what it could not verify. Almost
always upstream changed `index.html` — a reformatted CSP tag, a moved `<script>`. Look at
`element-web-dist/index.html` and adjust the step; the assertion is telling the truth.

**A shim reports `installed: false`.** It ran and did not attach. Its `detail` says what it
was trying to replace; upstream or the browser has moved it. This is our side to fix.

**A shim is missing from the report entirely.** The injection did not take at all. That is a
patch-script problem, not a shim problem — check the marker is in `index.html`.

**Everything installs and calls do not work.** Then the shims are attached to APIs whose
*contract* has changed, which no static check can see. Compare a working version against the
new one in the logs. This is what happened at v1.12.25.

**Reverting.** `just element-web-sync <old-version>`. `fetch-element-web.sh` wipes and
re-downloads `element-web-dist/`, so the revert is a re-fetch and not an undo — nothing you
hand-edited in there survives, by design.

## What is known about v1.12.25

Attempted on 2026-08-08 and reverted. Every patch assertion passed, every shim installed in
both documents, and the full Playwright suite passed 21 tests — real Element Web without our
shims works fine on it.

Elementium's own media transport does not. The new livekit-client never calls
`setLocalDescription` on our `RTCPeerConnection` shim (8 calls on v1.12.11, 0 on v1.12.25),
so the offer is never sent and the connection times out with "could not establish pc
connection". The handshake reports `protocol: 17`, so the client has moved.

Full detail in `specs/007-element-web-upgrade/spec.md`. The work is tracked as T024 and
blocks the upgrade.

## What the build was made from

`element-web-dist/.elementium-build.json`, written by the patch script and logged at startup:
version, source, an ISO8601 UTC timestamp, carried patches, a fingerprint of Element Call's
assets, and whether the autojoin test driver was injected.

That last field is a release gate, not a diagnostic. The autojoin driver logs in as a test
user from a token baked into the page and dials a call on startup; `scripts/prepare-build.sh`
refuses to build when it is present.
