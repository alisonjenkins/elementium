# Contract: the upstream surface we depend on

**Verified against**: Element Web v1.12.11 (current) and v1.12.25 (target), 2026-08-08
**Asserted by**: `frontend/tests/matrixrtc/shim-contract.spec.ts` (T006)

These are other people's internals. Nothing here is a published API, and every item is
something an upgrade can remove without notice and without an error. Writing them down is
what turns "the upgrade broke something" into "the upgrade changed item 4".

| # | Surface | Depended on by | v1.12.11 | v1.12.25 |
|---|---|---|---|---|
| 1 | `index.html` contains a CSP `<meta http-equiv="Content-Security-Policy">` | `patch-element-web.sh` removes it; Tauri's CSP is the boundary | present | present |
| 2 | `index.html` contains at least one `<script>` to inject before | shims injection | present | present |
| 3 | `widgets/element-call/` exists with its own `index.html` | widget shim + IPC bridge injection | present | present |
| 4 | livekit worker message `{kind: "setKey", data: {participantIdentity, key, keyIndex}}` | `e2ee-bridge.ts` hooks `Worker.prototype.postMessage` | present | present |
| 5 | `window.mxMatrixClientPeg.get()` | Playwright harness (room encryption assertions) | present | present |
| 6 | `config.json` keys we set are read | `element-web-config/*.json` | 9/9 | 9/9 |
| 7 | Element Call is bundled in the release, not pinned separately | everything about calls | true | true |

## Notes that matter more than the table

**Item 4 nearly went down as broken.** A grep for `kind:"setKey"` finds four hits in
v1.12.11 and none in v1.12.25 — which reads exactly like a changed worker protocol and a
dead E2EE bridge. It has not changed. The v1.12.25 bundle is minified with template
literals:

```js
{kind:`setKey`,data:{participantIdentity:r,isPublisher:...,key:n,keyIndex:...}}
```

So the assertion in T006 must observe the message at runtime, not grep the bundle. A
build-artefact grep is the wrong instrument for this contract and gives a confidently wrong
answer.

**Item 6**: `config.sample.json` has identical keys in both versions. Our
`bug_report_endpoint_url` is absent from the sample in both but is read by the bundle in
both — absence from the sample is not absence from the schema.

**Item 7** is an assumption, not a guarantee. If Element Call is ever shipped separately it
becomes a second version to track, which is why the build record fingerprints its assets
(`data-model.md`) rather than trusting the release version to describe it.

## How to re-verify

Items 1, 2, 3, 6, 7 are structural and can be checked against an extracted tarball without
running anything. Items 4 and 5 require a running client and belong in the Playwright
contract test, because both are runtime facts and one of them is invisible to static
inspection.
