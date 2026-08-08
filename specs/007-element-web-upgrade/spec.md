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

## Finding — 2026-08-08: the upgrade was attempted, and it is blocked on one thing

The pin was moved to v1.12.25 and moved back. Everything the reading predicted held; one
thing it did not predict does not work, and it is not in any of the surfaces that were
checked.

### What held

| Checked | Result on v1.12.25 |
|---|---|
| Every patch step's assertion | passed — CSP removed, both injections took |
| Shim contract (4 checks) | passed, in the main window and the widget frame |
| Full Playwright suite | **21 passed**, including all five Element Call scenarios and the fourth-joiner rotation |
| Config keys, dist layout, `setKey` worker message | unchanged, as predicted |

So the entire structural analysis was right, and the Playwright participants — real Element
Web without our shims — work perfectly on the new version. The upgrade is not blocked by
Element Web.

### What does not work

Elementium itself cannot establish a peer connection:

```
[CallViewModel][ConnectionManager] livekitRoom.connect FAILED ws://localhost:7880
    ConnectionError: could not establish pc connection
Error boundary caught: Error: Failed to connect to Livekit server
```

Everything up to that point succeeds. The JWT is obtained, the WebSocket opens, and the SFU
answers: `connected to Livekit Server ... version: 1.13.5, protocol: 17`. Element Call
reaches "in the call". Only the media transport fails.

The measurement that localises it:

| | v1.12.11 | v1.12.25 |
|---|---|---|
| `setLocalDescription` calls on our shim | 8 | **0** |
| `setRemoteDescription` calls | 8 | **0** |
| `createOffer` calls | — | 3, each returning ~2.8 kB of SDP |
| Outbound audio | 5,250 of 5,250 sent | 766 of 6,500, then a closed channel |

We create the peer connections and produce offers. The new livekit-client **never calls
`setLocalDescription`**, so the offer is never sent, no answer ever arrives, and the
connection times out. The failure is therefore in our `RTCPeerConnection` shim's API
surface, at some call the new client makes *before* `setLocalDescription` — not in the
offer we generate, which it never asks for.

`protocol: 17` in the handshake is the visible sign that livekit-client moved. Which call
it now makes first, and what it expects back, is the next thing to find out: instrument the
shim to log every method and property access, and compare the two versions' call sequences
directly.

### Narrowed, 2026-08-08: it is not an exception, it is a protocol change

The peer-connection trace (`ELEMENTIUM_TRACE_PC=1`) records every property read and method
call in order. Per connection, on v1.12.25:

```
createDataChannel ×3
addTransceiver ×6
signalingState -> stable
createOffer(0 args)
   ← and nothing further, ever
```

`createOffer` is entered 3 times and **resolves 3 times**. `getTransceivers` is called
**zero** times, and `setLocalDescription` zero times. That places the stall between
`createOffer` resolving and `getPlaceholderMids()`, which is livekit's SDP munging — so the
munging was the obvious suspect.

**It is not.** Our real offer was run through livekit's own munge block —
`sdp-transform`'s `parse`, `ensureIPAddrMatchVersion`, `ensureAudioNackAndStereo`, `write` —
outside the browser. It completes cleanly:

```
section type=application mid=ofv rtp=0 fmtp=0
section type=audio       mid=ATe rtp=1 fmtp=1
section type=video       mid=Xpg rtp=2 fmtp=1
munge ok
```

What the trace shows instead is the order of events, and it has changed:

```
Created SDP offer
signal connecting to ws://localhost:7880/rtc/v1?access_token=…&join_request=CAES…
signal connected
```

**The offer is created before the signalling socket opens, and travels inside it.** In
livekit protocol 17 the client builds a `JoinRequest` carrying a `publisherOffer` and sends
it as a base64 query parameter on the WebSocket URL (`SignalClient.ts`,
`createJoinRequestConnectionParams`). The old flow — connect, then offer, then
`setLocalDescription`, then exchange over the socket — is gone.

So nothing is throwing and nothing is missing from our API surface. We are answering the
calls of a handshake that no longer applies. `setLocalDescription` is never called because
in this flow the client waits for the join response to carry the answer, and then applies
both.

That reframes T024. It is not "add a missing method"; it is a new connection flow to
support, and the next thing to establish is what the SFU sends back in the join response to
our offer — which needs the signalling messages logged, not the peer connection.

Also visible in the trace, and worth noting before it is mistaken for a fault: our offer is
now `a=recvonly` on both media lines where it used to be `a=sendrecv`. That is the same
protocol-17 change — livekit pre-populates recvonly *placeholder* transceivers and replaces
tracks into them later, which is what `placeholderMidsFromTransceivers` exists for.

### What this changes about the plan

Nothing structural, and the ordering was right: the instrumentation from Phases 1 and 2 was
in place before the pin moved, so the failure was localised from logs in one run rather than
bisected. The pin is back at v1.12.11 and the working tree runs it.

It does add a task the plan did not have — the shim's `RTCPeerConnection` has to be brought
up to what livekit-client now expects — and that is a bigger piece of work than the upgrade
it blocks.

## Finding — 2026-08-08: the cause was ours, and it was a misspelled property

The signalling trace answered the question the previous finding left open. What the SFU
replies to an offer carried in the join request is:

```
[LKSignal] connect: join_request 1023 bytes, compression=GZIP
[LKSignal] recv #0 join (659 bytes)
[LKSignal] send #0 add_track (46 bytes)
[LKSignal] recv #1 refresh_token (463 bytes)
[LKSignal] recv #2 leave reason=STATE_MISMATCH action=RECONNECT (6 bytes)
```

No answer, ever. `STATE_MISMATCH` is the SFU refusing the negotiation, and the LiveKit
server log says exactly what it objected to:

```
received offer  {"offer": {"type":"unknown", "sdp":"v=0\r\no=str0m-0.16.2 ..."}}
```

**The offer had no type.** `create_offer` returns a Rust `SessionDescription`, whose field
is renamed to `type`; the shim declared the IPC response as `{ sdpType, sdp }` and read
`desc.sdpType`, a property that has never existed on it. Every description handed to
livekit-client carried `type: undefined`, proto3 dropped the empty field, and the SFU got
an untyped offer. Three call sites had it, including the answer returned by
`set_remote_description`, which fed an untyped `localDescription`.

This was not new, and not upstream's doing. It has been true the whole time. On protocol 16
the WebSocket shim injects the missing `type` back into outgoing offers and answers — which
is precisely what that block of code was written for, and its comment says so. Protocol 17
carries the publisher offer in the connection URL, where the injection cannot reach it, and
the latent bug became a hard failure.

So the previous finding's framing — "a new connection flow to support" — was wrong in the
part that mattered. The flow needed no support. It removed the cover from a defect of ours.

### What works now on v1.12.25

With the type read correctly, the SFU answers: `answer` (2,520 bytes), then trickle,
`track_published`, `subscription_response`, and a steady ping/pong. 16,750 frames captured,
16,749 sent, one dropped to a channel closed during an early reconnect. E2EE keys exchanged
both ways (113 key events), and the decrypt failures are all in the first seconds before any
key is known.

### What still does not work

**Zero inbound Opus frames decoded.** The SFU's answer offers the remote tracks
(`a=sendonly` with real msids) and we subscribe, but nothing is decoded from them. Publishing
works; receiving does not. That is the remaining blocker on the upgrade, and it is a
different problem from the one this finding closes — the negotiation now succeeds.

Counting the inbound signalling kinds says where to look next:

| kind | count |
|---|---|
| `subscription_response` | 420 |
| `pong` / `pong_resp` | 69 each |
| `trickle` | 39 |
| `media_sections_requirement` | 6 |
| `answer` | 3 |
| **`offer`** | **0** |

**The SFU never sends a subscriber offer.** On protocol 16 that offer is how the remote
tracks get their `m=` sections; here it never comes, so no receive section is ever
negotiated and there is nothing for inbound RTP to arrive on. The 420 `subscription_response`
messages are the client asking again and again for tracks it has nowhere to put.

`media_sections_requirement` is the replacement, and livekit-client's source says exactly
what it does with it:

```js
onMediaSectionsRequirement = requirement => {
  this.addMediaSections(requirement.numAudios, requirement.numVideos);
  this.negotiate();
};

addMediaSections(numAudios, numVideos) {
  const transceiverInit = { direction: 'recvonly' };
  for (let i = 0; i < numAudios; i++)
    this.pcManager?.addPublisherTransceiverOfKind('audio', transceiverInit);
  for (let i = 0; i < numVideos; i++)
    this.pcManager?.addPublisherTransceiverOfKind('video', transceiverInit);
}
```

**Protocol 17 receives on the publisher connection.** One `recvonly` section per remote
track, added to the connection we already have, then a re-offer. There is no subscriber
offer because there is no longer a subscriber negotiation for these — which is why looking
for one found nothing.

### Why nothing decodes, and it is ours again

`create_offer` in `crates/elementium-webrtc/src/peer_connection.rs` keeps **one audio mid
and one video mid** per connection, and skips any transceiver whose kind already has one:

```rust
let existing = match tc.kind {
    MediaKind::Audio => pc.audio_mid,
    MediaKind::Video => pc.video_mid,
};
if let Some(mid) = existing {
    tracing::debug!(..., "Transceiver already present, not re-adding");
    continue;
}
```

So when livekit-client asks for one recvonly audio section per remote participant, we add
the first and silently drop the rest. The offer carries a single audio m-line, the SFU has
nowhere to put the other streams, and no inbound audio is decoded — the symptom exactly.

That guard was added for a real reason (re-offering appended a duplicate m-line for a track
that already existed), so the fix is not to delete it. It has to tell "the same transceiver,
offered again" from "another transceiver of the same kind", which means tracking the mids
added per kind rather than a single mid per kind. `audio_mid` and `video_mid` are also the
mids we *send* on, in `write_audio`/`write_video`, so they have to keep meaning that.

This is stated as the cause on the strength of the code and the counts, not on a run: it
has not yet been fixed and re-measured. That is the next step, and it needs care, because
getting it wrong silently breaks the send path that works today on v1.12.11.

Routing is not the problem: inbound media is dispatched by `mid`, not by SSRC, so the
missing `a=ssrc` lines in the protocol-17 answers are a red herring.

The pin is back at v1.12.11 until inbound audio works, because a pinned version whose calls
are half-deaf is worse than one that is known to work.

### Two things this cost, that are worth naming

The trace was logging the full WebSocket URL on open, opened, close and error — four times
per connection, including `access_token`, a JWT granting publish and subscribe on the room.
That predates this work; it was visible because reading the trace put it in front of me.

And the previous session's `[LKSignal]` predecessor dumped whole message bodies as byte
arrays for the first five messages in each direction. Kinds and sizes answered the question
in one run; the bytes never did.

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

1. ~~**Fork hosting.**~~ **Answered 2026-08-08: a public fork on the user's own account** —
   `alisonjenkins/element-web`, long-lived branch `elementium`, rebased onto each upstream
   tag. Public is not a preference here: Element Web is AGPL-3.0 and Elementium declares
   `AGPL-3.0-or-later`, so once a *source* patch ships we are distributing a modified AGPL
   work and its corresponding source has to be available to the people receiving it. A
   public fork satisfies that by existing.

   **The fork already exists**, from an earlier attempt to contribute. Inspected rather than
   assumed: public, `develop` **identical** to upstream's, no other branches, so the patch
   branch starts from a clean base and disturbs nothing.

   One thing that inspection turned up and would otherwise have surfaced as a confusing
   error: **the fork has no tags.** GitHub does not copy them. The patch branch rebases onto
   upstream *release tags*, so the tooling needs two remotes — the fork for the branch,
   `element-hq/element-web` for the tags. A rebase that fetched only from the fork fails with
   `invalid upstream 'v1.12.25'`, which reads like a typo.

   Two consequences that are not ours to arrange:

   - Upstream requires a **CLA**, not a DCO (`cla-assistant.io/element-hq/element-web`). It
     is signed once, personally, and is a precondition for the contributing half of this
     feature. No tooling can stand in for it.
   - This repository declares `AGPL-3.0-or-later` in `Cargo.toml` and has **no `LICENSE`
     file**, so GitHub reports it as unlicensed. Unrelated to the fork and worth fixing
     regardless.
2. **Node 24.** Upstream builds on 24 and we provide 22.23.2. Settled by building, not by
   reading version ranges.
3. **Is a source build needed at all before the first patch exists?** The patch set is
   empty today. There is a case for landing the upgrade and the shim-contract tests first,
   and the fork mechanism only when there is something to carry — and a case against,
   which is that a mechanism nobody has exercised is a mechanism that does not work. This
   is the main sequencing decision in the feature.
