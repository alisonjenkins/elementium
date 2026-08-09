#!/usr/bin/env bash
# Generate and configure the test homeserver, idempotently.
#
# Everything this applies used to be a hand edit to a gitignored file, which meant the
# environment could not be rebuilt from a clean checkout: `docker compose up` produced a
# default synapse that no part of the stack could work with, and the reasons were spread
# across a README. Each setting below has a failure it prevents, named where it is applied.
#
# Safe to re-run. Run before `docker compose up`; the config must exist before synapse starts.
set -euo pipefail

cd "$(dirname "$0")"

SYNAPSE_IMAGE="${SYNAPSE_IMAGE:-matrixdotorg/synapse:latest}"
DATA="$PWD/synapse"

mkdir -p "$DATA"

# Synapse's own image, rather than anything on the host: it has the python and the yaml
# parser, and editing a config with sed is how a config ends up subtly wrong.
in_synapse() {
    # `-i` matters: the configuration step is fed a script on stdin, and without it docker
    # attaches nothing, python reads an empty program, exits 0, and the config is silently
    # left unchanged.
    docker run --rm -i -v "$DATA:/data" -e SYNAPSE_SERVER_NAME=localhost \
        -e SYNAPSE_REPORT_STATS=no -e UID="$(id -u)" -e GID="$(id -g)" \
        --entrypoint "$1" "$SYNAPSE_IMAGE" "${@:2}"
}

if [[ ! -f "$DATA/homeserver.yaml" ]]; then
    echo "[configure] generating a fresh homeserver config"
    docker run --rm -v "$DATA:/data" -e SYNAPSE_SERVER_NAME=localhost \
        -e SYNAPSE_REPORT_STATS=no -e UID="$(id -u)" -e GID="$(id -g)" \
        "$SYNAPSE_IMAGE" generate >/dev/null
fi

# A self-signed certificate for the federation listener. lk-jwt-service authenticates a
# participant by calling the federation OpenID userinfo endpoint the way any server would --
# `matrix://localhost`, which resolves to port 8448 over TLS. Without it every participant is
# refused an SFU token and the only symptom is that calls never connect.
if [[ ! -f "$DATA/localhost.tls.crt" ]]; then
    echo "[configure] generating a self-signed federation certificate"
    in_synapse openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
        -keyout /data/localhost.tls.key -out /data/localhost.tls.crt \
        -subj "/CN=localhost" >/dev/null 2>&1
    # openssl runs as root in that container and writes the key 0600 root:root, while synapse
    # runs as the invoking user (UID/GID in the compose file, so the config stays editable
    # from outside). The homeserver then cannot read its own federation key and exits at
    # startup with `Error accessing file '/data/localhost.tls.key'` -- on a *clean checkout
    # only*, which is why it survived: every existing environment already had a key from
    # before the container ran as anyone but root.
    in_synapse chown "$(id -u):$(id -g)" /data/localhost.tls.key /data/localhost.tls.crt
fi

echo "[configure] applying test-environment settings"
in_synapse python - <<'PY'
import yaml

PATH = "/data/homeserver.yaml"
with open(PATH) as f:
    cfg = yaml.safe_load(f)

# The client API moves off 8008 so the RTC-transport proxy can own it. Element Web decides
# whether Element Call is usable by calling an MSC4143 endpoint synapse does not implement,
# and does not fall back to the `.well-known` entry carrying the same information -- so
# without the proxy every call here silently became a Jitsi call.
cfg["listeners"] = [
    {
        "port": 8108,
        "type": "http",
        "tls": False,
        "x_forwarded": True,
        "resources": [{"names": ["client", "federation"], "compress": False}],
    },
    {
        "port": 8448,
        "type": "http",
        "tls": True,
        "x_forwarded": True,
        "resources": [{"names": ["federation"], "compress": False}],
    },
]
cfg["tls_certificate_path"] = "/data/localhost.tls.crt"
cfg["tls_private_key_path"] = "/data/localhost.tls.key"

# The harness creates participants on demand; a registration flow with verification stages,
# or a rate limiter, turns a test suite into a flaky one.
cfg["enable_registration"] = True
cfg["enable_registration_without_verification"] = True
for name, values in {
    "rc_message": {"per_second": 1000, "burst_count": 1000},
    "rc_registration": {"per_second": 1000, "burst_count": 1000},
    "rc_joins": {"local": {"per_second": 1000, "burst_count": 1000},
                 "remote": {"per_second": 1000, "burst_count": 1000}},
    "rc_invites": {"per_room": {"per_second": 1000, "burst_count": 1000},
                   "per_user": {"per_second": 1000, "burst_count": 1000}},
}.items():
    cfg[name] = values
cfg["rc_login"] = {
    "address": {"per_second": 1000, "burst_count": 1000},
    "account": {"per_second": 1000, "burst_count": 1000},
    "failed_attempts": {"per_second": 1000, "burst_count": 1000},
}

# Required for synapse to serve `/.well-known/matrix/client` at all, which is how clients
# discover both the homeserver and the RTC focus.
cfg["public_baseurl"] = "http://localhost:8008/"
cfg["serve_server_wellknown"] = True
cfg["extra_well_known_client_content"] = {
    "org.matrix.msc4143.rtc_foci": [
        {"type": "livekit", "livekit_service_url": "http://localhost:8090"}
    ]
}

with open(PATH, "w") as f:
    yaml.safe_dump(cfg, f, default_flow_style=False, sort_keys=False)
print("[configure] homeserver.yaml written")
PY
