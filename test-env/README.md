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

## What is not here yet

The Elementium app itself still points at whatever homeserver its config names.
Pointing it here — and driving further participants with Playwright — is the next
step, and is what makes the two faults above reproducible on demand.
