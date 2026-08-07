#!/usr/bin/env bash
# Leave and forget every room the test participants are in.
#
# Each `provision.sh` creates a room, and each call test creates another, so a few runs leave
# a client showing dozens of rooms called "Elementium call test", "joins last", "one leaves"
# and so on. They are harmless to the server and unreadable to a person, which makes anything
# observed by looking at the client that much harder.
#
# Leave *and* forget: leaving alone keeps them in the room list as "historical".
#
# Refuses to run against anything but the local test homeserver. The whole point is that it
# empties an account's room list, which is not something to do by accident to a real one.
set -euo pipefail

HS="${HS:-http://localhost:8008}"
COUNT="${COUNT:-4}"

case "$HS" in
    http://localhost:8008 | http://127.0.0.1:8008) ;;
    *)
        echo "refusing to run against $HS -- this empties a room list, and is only for the" >&2
        echo "local test homeserver." >&2
        exit 1
        ;;
esac

total=0
for i in $(seq 1 "$COUNT"); do
    token=$(curl -s -X POST "$HS/_matrix/client/v3/login" \
        -H 'Content-Type: application/json' \
        -d "{\"type\":\"m.login.password\",\"identifier\":{\"type\":\"m.id.user\",\"user\":\"tester$i\"},\"password\":\"test-password-$i\"}" \
        | python3 -c 'import sys,json; print(json.load(sys.stdin).get("access_token",""))')
    [[ -n "$token" ]] || continue

    rooms=$(curl -s "$HS/_matrix/client/v3/joined_rooms" -H "Authorization: Bearer $token" \
        | python3 -c 'import sys,json; print(" ".join(json.load(sys.stdin).get("joined_rooms",[])))')

    n=0
    for room in $rooms; do
        curl -s -o /dev/null -X POST "$HS/_matrix/client/v3/rooms/$room/leave" \
            -H "Authorization: Bearer $token"
        curl -s -o /dev/null -X POST "$HS/_matrix/client/v3/rooms/$room/forget" \
            -H "Authorization: Bearer $token"
        n=$((n + 1))
    done
    total=$((total + n))
    echo "[cleanup] tester$i left and forgot $n rooms"
done
echo "[cleanup] $total room memberships cleared"
