#!/usr/bin/env bash
# Create test participants and a call room on the local homeserver.
#
# Idempotent: re-registering an existing user falls back to logging in, so this can
# be re-run between test runs without tearing the stack down. Synapse's database is
# the slowest part to rebuild and there is no reason to.
#
# Prints a JSON blob the harness consumes: user ids, access tokens, device ids and
# the room id.
set -euo pipefail

HS="${HS:-http://localhost:8008}"
COUNT="${COUNT:-3}"

register() {
  local user="$1" pass="$2" body
  # v3 register with `dummy` auth: verification is disabled in this homeserver's
  # config, so there is no email or captcha stage to satisfy.
  body=$(curl -s -X POST "$HS/_matrix/client/v3/register" \
    -H 'Content-Type: application/json' \
    -d "{\"username\":\"$user\",\"password\":\"$pass\",\"auth\":{\"type\":\"m.login.dummy\"},\"inhibit_login\":false}")
  if echo "$body" | grep -q '"access_token"'; then
    echo "$body"
    return
  fi
  if echo "$body" | grep -q 'M_USER_IN_USE'; then
    curl -s -X POST "$HS/_matrix/client/v3/login" \
      -H 'Content-Type: application/json' \
      -d "{\"type\":\"m.login.password\",\"identifier\":{\"type\":\"m.id.user\",\"user\":\"$user\"},\"password\":\"$pass\"}"
    return
  fi
  echo "registration failed for $user: $body" >&2
  exit 1
}

declare -a USERS=()
for i in $(seq 1 "$COUNT"); do
  USERS+=("$(register "tester$i" "test-password-$i")")
done

token_of() { python3 -c 'import sys,json; print(json.load(sys.stdin)["access_token"])' <<<"$1"; }

OWNER_TOKEN=$(token_of "${USERS[0]}")

# One room shared by every participant, created by the first and joined by the rest.
ROOM=$(curl -s -X POST "$HS/_matrix/client/v3/createRoom" \
  -H "Authorization: Bearer $OWNER_TOKEN" -H 'Content-Type: application/json' \
  -d '{"preset":"public_chat","name":"Elementium call test"}' \
  | python3 -c 'import sys,json; print(json.load(sys.stdin)["room_id"])')

for u in "${USERS[@]:1}"; do
  curl -s -o /dev/null -X POST "$HS/_matrix/client/v3/join/$ROOM" \
    -H "Authorization: Bearer $(token_of "$u")"
done

python3 - "$ROOM" "${USERS[@]}" <<'PY'
import sys, json
room = sys.argv[1]
users = [json.loads(u) for u in sys.argv[2:]]
print(json.dumps({
    "homeserver": "http://localhost:8008",
    "room_id": room,
    "participants": [
        {"user_id": u["user_id"], "access_token": u["access_token"], "device_id": u["device_id"]}
        for u in users
    ],
}, indent=2))
PY
