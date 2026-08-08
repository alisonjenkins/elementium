# Feature Specification: Element Web upgrade and patch maintenance

**Created**: 2026-08-08
**Status**: Draft — upstream probed, three blockers measured
**Input**: "Spec out the upgrade to the latest version of Element Web. We need to be
careful not to break our project, and that the shims successfully install on the new
version. Make it easy to stay in sync. I want to contribute back to Element Web, but also
maintain patches where we implement functionality ahead of what they have."

## Why this exists

We are pinned to Element Web **v1.12.11**. The latest release is **v1.12.25**, published
2026-08-05 — fourteen releases and several months behind. Nothing forces the upgrade
today, and that is the problem: the longer the gap, the larger the single step, and the
harder it is to tell an upstream change from one of ours when something breaks.

Underneath the version number there are two things this feature is really about.

**We have no way to change Element Web itself.** Every modification we make today is
either a runtime monkey-patch of a browser global, or `sed`/`awk` surgery on the built
`index.html`. Neither can express a change to Element Web's own behaviour, and neither
can be sent upstream — a patch to a minified bundle is not a contribution.

**We have no way to carry a change upstream has not taken yet.** That is the normal
condition for a downstream that moves faster in one area, and it needs a mechanism before
it needs a policy.

## What is true today

| | |
|---|---|
| Pinned version | v1.12.11, via release tarball (`ELEMENT_WEB_SOURCE=release`) |
| Where it lands | `element-web-dist/`, git-ignored, rebuilt by `scripts/fetch-element-web.sh` |
| How we modify it | `scripts/patch-element-web.sh`: copy shims, copy config, delete the CSP meta tag, inject a `<script>` before the first one — in `index.html` and in `widgets/element-call/index.html` |
| What the shims do | Replace `RTCPeerConnection`, `navigator.mediaDevices`, `Storage.prototype`, `Worker.prototype.postMessage`, `WebSocket` — 2,700 lines across nine modules |
| Source checkout | None. `ELEMENT_WEB_SOURCE=git` exists but is unused |
| Patches against Element Web source | None. There is nowhere to put one |

The shims are not a stopgap to be removed. They are how a browser application reaches
native audio, video and key storage, and they patch *browser* APIs rather than Element
Web internals — which is why they have survived this long unattended.

## Finding — 2026-08-08: what actually breaks, measured against v1.12.25

The v1.12.25 tarball and the upstream tree at that tag were both read before writing any
of this, because "be careful not to break things" is only actionable once the list of
things that break is a list.

### The dist layout is unchanged, and so is every contract the shims rely on

| Checked | v1.12.11 | v1.12.25 |
|---|---|---|
| `widgets/element-call/` present | yes | yes |
| CSP `<meta>` in `index.html` | yes | yes |
| A first `<script>` to inject before | yes | yes |
| `config.sample.json` keys | — | **identical**, none added or removed |
| Our nine config keys still read | yes | yes (`bug_report_endpoint_url` is absent from the sample but still read by the bundle) |
| livekit worker message `{kind: "setKey", data: {participantIdentity, key, keyIndex}}` | yes | yes |
| `failureTolerance` default 10, `-1` only for the external provider | yes | yes |

So the injection mechanics and the E2EE bridge — the two things most likely to fail
silently — should both survive the upgrade.

One near-miss is worth recording, because it nearly went into this document as a fact.
A first pass grepping the v1.12.25 bundle for `kind:"setKey"` found nothing while the
v1.12.11 bundle had it four times, which reads exactly like "the worker protocol
changed and our E2EE bridge is dead". It has not changed. The new bundle is minified with
template literals — <code>{kind:\`setKey\`,...}</code> — so a quote-sensitive search
misses it. The contract is identical. Checked twice because the first answer was the
alarming one, and an alarming answer deserves the second look more than a reassuring one
does.

### Element Web is now a monorepo, and our `git` mode cannot build it

This is the real blocker, and it is not about the version number at all.

At v1.12.25 the repository root is `element-web-monorepo`: an **nx** workspace with
`apps/web`, `apps/desktop`, `packages/*` and `modules/`. There is **no `yarn.lock`** and
no root `build` script; it is a **pnpm** workspace (`pnpm-lock.yaml`,
`pnpm-workspace.yaml`), and the web application builds with `nx build` from
`apps/web`.

`fetch_git()` in `scripts/fetch-element-web.sh` does:

```bash
yarn install --frozen-lockfile
yarn build
cp -r "$cache_dir/webapp" "$DIST_DIR"
```

Every one of those three lines is wrong for the current upstream: wrong package manager,
no such script, and the output is no longer at `webapp/`. The path we would need for both
contributing back *and* carrying patches is the one path that does not work.

It has presumably not worked for some time. Nothing uses it, so nothing said so.

### Toolchain gap

Upstream's `.node-version` says **24**; `package.json` engines say `>=22.18`. Our dev
shell provides node **22.23.2** and pnpm 11.18.0. Within the stated engine range, below
what upstream builds with. Whether that matters is not established and should be settled
by building, not by reasoning.

### Upstream patches its own dependencies

The upstream tree has a `patches/` directory of its own. Whatever we adopt should not
collide with it, and it is a useful precedent for what the project considers normal.

## The shape of the answer

Three kinds of change, told apart by intent, with a different home and a different cost
for each. The point of naming them is that the wrong home is what makes patch sets
unmaintainable: a host-integration hack living as a source patch has to be rebased
forever and can never be contributed, and a genuine product change living as a runtime
monkey-patch can never be contributed either.

### 1. Host integration → stays a runtime shim

Native WebRTC, native capture devices, keyring-backed secret storage, the Tauri IPC
bridge. These patch browser globals, not Element Web, so upstream churn barely touches
them — which the table above bears out. They are also not upstream-shaped by nature:
"replace `RTCPeerConnection` with one that talks to a Rust process over IPC" is not a
contribution Element Web would take, and should not be offered.

**Rule**: if it would still make sense in a browser that had our native backend, and
nowhere else, it is a shim.

### 2. Product changes → an atomic commit on a rebased fork branch

Anything a person would recognise as an Element Web feature or fix. These live as
commits on a long-lived branch of a fork, rebased onto each upstream tag, one commit per
logical change — the same rule this repository already applies to itself.

Contributing back is then `git cherry-pick` onto a fresh branch from upstream, and a pull
request. When it merges, the commit disappears from our branch at the next rebase,
because rebase drops commits whose patch-id upstream already has. **The mechanism for
contributing and the mechanism for carrying are the same mechanism**, which is what makes
it scale: there is no translation step, and no separate format to keep in sync.

The alternative — a `patches/*.patch` series applied at build time, Debian-style — was
considered and is worse *for this purpose*. It keeps the patches in our own repository,
which is genuinely nicer for review, but refreshing a patch against a moved upstream is
manual and lossy, per-patch authorship and history are gone, and contributing back means
reconstructing a commit from a diff. It optimises for the case where you never intend to
upstream anything, which is the opposite of what was asked for.

**Rule**: if you would be willing to open the pull request today, it is a commit on the
fork branch — whether or not you actually open it.

### 3. Build-time surgery → stays a script, but must fail loudly

Removing the CSP meta tag and injecting the shims script cannot be done any other way
against a prebuilt tarball, and doing it from source would mean carrying a patch for
something that is purely packaging.

What has to change is the failure mode. `sed` that matches nothing and `awk` that finds
no `<script>` both exit 0 today, so an upstream change to `index.html` produces a build
that succeeds and an application that is broken in a way nothing reports. Every step must
assert its effect.

## User Scenarios

### US1 (P1) — The upgrade lands without breaking the application

A maintainer moves the pinned version to the latest release and can tell, before shipping
it, whether audio, video, encryption and the shims all still work.

**Independent test**: bump the pin, run the automated checks, and get a verdict naming
what broke rather than "it built".

### US2 (P1) — The shims are proven to install, not assumed to

Each shim asserts that it took effect on the version in front of it, and a version bump
that silently disables one fails a test rather than reaching a user.

**Independent test**: with a deliberately broken injection, the check fails and names the
shim that did not install.

### US3 (P2) — Staying in sync is one command

Following upstream is routine rather than a project. A maintainer runs one command with a
version, and gets either a clean result or a specific list of conflicts.

**Independent test**: sync to a version two releases ahead and read the report.

### US4 (P2) — A change can be carried, and offered upstream, without duplication

A change we make to Element Web can be sent as a pull request and carried locally until
it merges, from a single source of truth, and disappears from our set once it lands.

**Independent test**: carry a change, produce a PR branch from it, simulate it landing
upstream, and confirm it drops out of our set on the next sync without manual editing.

### US5 (P3) — What we are carrying is visible

Anyone can see the current patch set, why each patch exists, and whether it is meant to
go upstream.

**Independent test**: read one file and answer "what are we carrying, and why" for each.

## Success Criteria

- **SC1**: The application runs on the latest Element Web release with audio and video
  working in both directions and E2EE keys exchanged — measured the way feature 003
  measures it, not by the app appearing to start.
- **SC2**: Every shim reports whether it installed, and a failure to install fails a test.
- **SC3**: Moving to a new upstream version is a single command plus a report; no step in
  it requires editing a script.
- **SC4**: A carried change can be turned into a pull request without being rewritten, and
  is dropped automatically once upstream has it.
- **SC5**: No step of the build can silently do nothing. Each patch operation asserts its
  own effect.
- **SC6**: The version currently in use, and the patches applied on top, are recorded in
  the build output and readable at runtime — so a bug report identifies what was running.

## Assumptions

- Element Web releases stay roughly two-weekly, so the sync path is exercised often enough
  to stay working. If it is used twice a year it will be broken twice a year.
- The release tarball remains the normal source, with the fork used when a patch is
  carried. Building from source on every developer machine is a cost worth avoiding while
  the patch set is empty.
- Contributing upstream means Element Web's own process: their code style, their tests,
  their sign-off requirements. This feature provides the mechanism, not an exemption.
- Element Call ships inside the Element Web release as `widgets/element-call`, and is not
  separately pinned. If that changes it becomes a second version to track.

## Open questions

1. **Fork hosting.** A fork of `element-hq/element-web` under the user's account is the
   obvious answer, but it is their account and their call, and it decides where CI for the
   patch branch would live.
2. **Node 24.** Upstream builds on 24 and we provide 22.23.2. Settled by building, not by
   reading version ranges.
3. **Is a source build needed at all before the first patch exists?** The patch set is
   empty today. There is a case for landing the upgrade and the shim-contract tests first,
   and the fork mechanism only when there is something to carry — and a case against,
   which is that a mechanism nobody has exercised is a mechanism that does not work. This
   is the main sequencing decision in the feature.
