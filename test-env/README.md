# A local MatrixRTC stack

Everything a call needs, on localhost: a homeserver, an SFU, and the service that
lets one authenticate to the other.

## Why this exists

Some faults cannot be reproduced without a real homeserver in the loop, because
Matrix is what drives them:

- **How long a key takes to arrive.** Keys travel as to-device messages. The delay
  is a property of the homeserver, the sender's rotation policy and the receiving
  client's sync loop — none of which exist with a bare SFU.
- **Why someone joining or leaving can silence everyone already in the call.**
  Element Call rotates its key on *every* leaver, and on a joiner when the current
  key is more than ten seconds old. Whether the rest of the room keeps hearing each
  other through that depends on the timing between distribution and use. It takes
  at least three participants and real membership events to see it at all.

The existing browser tests (`frontend/tests/browser/`) point a Chromium at a bare
SFU, which is right for measuring what a receiver makes of our media. It cannot
reach either of the above.

## What is in it

| Service | Port | Why |
|---|---|---|
| Synapse | 8008 (client + federation), 8448 (federation, TLS) | Accounts, rooms, membership, to-device key delivery |
| LiveKit | 7880 | The SFU |
| lk-jwt-service | 8090 | Exchanges a Matrix OpenID token for a LiveKit JWT |

Everything binds to localhost, and not only for convenience: browsers require a
secure context for insertable streams, which back livekit's E2EE worker, and
`http://localhost` counts as secure where a LAN address does not.

## Running it

For tests, nothing: `pnpm exec playwright test` (or `just test-browser`) brings the
stack up and takes it down again. If it was already running it is left alone, so a
test run cannot destroy an environment someone was using.

For the app:

```sh
just dev-test-env       # stack up, Element Web pointed here, participants created
just test-env-down      # stop it
```

Log in as `tester1` / `test-password-1`. The app and any Playwright participants then
share a homeserver — without that they are on different ones and never see each
other, which looks exactly like a broken call.

By hand:

```sh
cd test-env
docker compose up -d
./provision.sh          # participants and a shared room; prints JSON
```

`provision.sh` is idempotent — re-registering an existing user falls back to
logging in — so it can be re-run between test runs without resetting the stack.

To check the whole chain is alive, ask for an SFU token the way Element Call does:

```sh
TOK=$(./provision.sh | python3 -c 'import sys,json;print(json.load(sys.stdin)["participants"][0]["access_token"])')
OID=$(curl -s -X POST http://localhost:8008/_matrix/client/v3/user/@tester1:localhost/openid/request_token \
        -H "Authorization: Bearer $TOK" -d '{}')
curl -s -X POST http://localhost:8090/sfu/get -H 'Content-Type: application/json' \
  -d "{\"room\":\"r\",\"openid_token\":$OID,\"device_id\":\"DEV1\"}"
```

A JWT with `canPublish: true` means Matrix authentication, federation and the SFU
are all working together.

## Things that were not obvious

Each of these presents as "calls do not connect", with nothing in the client to say
why, so they are written down rather than rediscovered.

- **The JWT service authenticates over federation, not the client API.** It calls
  `/_matrix/federation/v1/openid/userinfo` at `matrix://<server>`, which resolves to
  port **8448 over TLS**. Synapse's generated config serves federation on 8008 in
  plaintext and nothing on 8448, so the lookup fails. Hence the second listener and
  the self-signed certificate; the service runs with TLS verification disabled,
  which is safe here and nowhere else.
- **`LIVEKIT_FULL_ACCESS_HOMESERVERS` must name the homeserver.** Unset, every
  participant is refused a token and the container says so once at startup and then
  looks healthy.
- **Port 8090, not 8080.** 8080 is commonly taken by a dev server already, and the
  clash surfaces as the container exiting quietly.
- **Synapse must run as the invoking user** (`UID`/`GID` in the compose file), or
  the config it generates is owned by a uid that cannot be edited without root.
- **`public_baseurl` is required** for synapse to serve `/.well-known/matrix/client`
  at all, which is how Element Call discovers the RTC focus.

## How the app is pointed here

`ELEMENTIUM_TEST_ENV=1` makes `scripts/patch-element-web.sh` deploy
`element-web-config/config.test-env.json` instead of the usual one. That is the only
difference; the normal config is untouched, so an ordinary `just dev` still talks to
whatever homeserver it always did.

## Driving Element Call

`frontend/tests/matrixrtc/call-faults.spec.ts` runs three real Element Web clients
through a real Element Call: joining in sequence, one leaving, and hanging up and
calling again. All of them pass, so neither reported fault is Element Call and Matrix
alone on this stack.

That leaves one configuration untested — Elementium as a participant — and Playwright
cannot drive it, because it is a Tauri application with a native WebRTC stack. So the
other side is supplied instead:

```sh
just call-peers      # tester2 and tester3 join a call and stay in it
just dev-test-env    # in another terminal; log in as tester1 and join
```

While it holds, each participant reports what it can hear and whose keys it holds, so
"did Elementium's key ever arrive" is answerable from the other side.

## The same thing, with nobody watching

```sh
just test-app-call
```

One command: stack up if it is not already, a real Element Web participant in a call,
Elementium joining it by itself under Xvfb, and assertions on what each end actually
decodes — `frontend/tests/matrixrtc/app-call.spec.ts`. A stack it did not start is left
running, as everywhere else here.

It needs a camera and a microphone on the host. Elementium publishes real capture; there
is no fake device on the native path the way Chromium has one, so this cannot run on a
machine with neither, and the webcam light comes on while it runs. Only the GUI is
headless.

Its last test — whether a participant who joins *after* Elementium can decrypt its media —
was written expecting to fail and does not. Element Call, not Elementium, distributes and
rotates the key here, and Elementium adopts each one it is handed. The suspected
"distributed once at join" fault therefore belongs to the native LiveKit path, which this
test does not exercise; the comment on the test says so.

## Things that were not obvious, part two

Everything below stopped a call from happening at all, and none of it pointed at
itself.

- **Synapse has no MSC4143 `rtc/transports` endpoint**, and Element Web does *not* fall
  back to the `org.matrix.msc4143.rtc_foci` entry in `.well-known` that carries the same
  information. Without an answer it logs one warning and quietly makes a **Jitsi** call.
  Hence the nginx in front of synapse, which answers that one endpoint and proxies
  everything else — so every URL, including `public_baseurl`, is unchanged.
- **Element Call is behind the `feature_group_calls` labs flag.** Without it the call
  button is there and does something else.
- **matrix-js-sdk discovers well-known from the server *name*, over https.** This
  homeserver is `localhost`, so that is `https://localhost/` on port 443, which nothing
  here serves. Element Call then refuses with `MISSING_MATRIX_RTC_FOCUS`, which reads as
  a server misconfiguration and is not one. The Playwright harness answers it per page;
  the application would need the same if run against this stack.
- **Call membership is a state event**, so members at the default power level 0 are told
  "You do not have permission to start video call". `provision.sh` grants exactly that
  event rather than promoting anyone.
- **The room must be encrypted.** Element Call performs frame encryption only in an
  encrypted room; in a plain one it skips key generation, distribution and rotation
  entirely. A call test in an unencrypted room exercises none of that and passes.
- **Reusing a device id with a discarded crypto store breaks decryption silently.** A
  fresh browser context around an old access token keeps the device and loses its Olm
  state, so others encrypt keys to a device that cannot read them: packets arrive, not
  one frame decrypts. It is indistinguishable from the fault being investigated, and it
  was reported as a reproduction of it for a while. `freshSessions` exists so no test
  does it by accident.

## Rebuilding from nothing

`configure-synapse.sh` generates the homeserver config and applies every setting above,
idempotently, and `global-setup.ts` runs it before the stack starts. The config used to
be hand edits to a gitignored file, which meant a clean checkout produced a homeserver
no part of the stack could work with, for reasons spread across this README.
