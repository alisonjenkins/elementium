# Observability & structured logging

Elementium logs structured JSON to stdout via `tracing`/`tracing-subscriber`. Every log line is
one JSON object; every line carries a `correlation_id` field (inside `span`/`spans`) that groups
all events belonging to the same logical operation.

## Reading logs

Pipe stdout through `jq` (or any JSON log tool):

```bash
cargo run -p elementium 2>&1 | tee /tmp/elementium.log
jq -r 'select(.fields.correlation_id != null) | .fields.correlation_id' /tmp/elementium.log | sort -u
jq -c --arg cid "<a-correlation_id>" 'select(.fields.correlation_id == $cid)' /tmp/elementium.log
```

The last command gives the full ordered timeline for one call/session/app-instance, across every
crate it touched.

### Correlation ID scopes

- **`app_instance`** — one per process lifetime, entered at startup in `main()`. Fallback scope
  for anything logged before a call/session exists (device enumeration, secret-store init).
- **`call`** — one per `getUserMedia` invocation (`get_user_media` in
  `src-tauri/src/commands/media_devices.rs`), covers audio/video capture → encode.
- **`peer_connection`** — one per `create_peer_connection`, covers ICE/DTLS lifecycle and the
  PC's I/O loop.
- **`session`** — one per `livekit_connect`, covers the LiveKit connect/room/publish lifecycle;
  reused (not re-minted) by `livekit_disconnect` and `publish_track` for the same room.

Spans nest: a `call` or `session` span's `correlation_id` shadows the `app_instance` root for
events emitted while it's active.

## Controlling verbosity

`RUST_LOG` (standard `tracing-subscriber::EnvFilter` syntax) controls verbosity per-crate/module,
no rebuild required:

```bash
RUST_LOG=elementium_webrtc=debug,info cargo run -p elementium
```

Default (no `RUST_LOG` set) is `info` for everything.

## Writing a test that asserts on logged behavior

Use `elementium-observability-test`'s `LogCapture` (a `dev-dependency` in `elementium-webrtc` and
`elementium`/src-tauri) instead of asserting only on a function's return value:

```rust
use elementium_observability_test::LogCapture;

let capture = LogCapture::new();
let result = capture.run(|| my_function_under_test(...));

let event = capture.find_event("some_event_name").expect("expected event to fire");
event.assert_field("reason", "expected_reason");
```

See `crates/elementium-webrtc/src/engine.rs`'s `encrypt_or_drop_emits_structured_warning_when_no_key_set`
and `src-tauri/src/commands/media_devices.rs`'s `zero_channels_is_clamped_without_panic_and_logged`
for worked examples.

## Rules for new logging call sites

- Structured fields only — `tracing::info!(device_id = %id, "message")`, never
  `tracing::info!("device {id} failed")`. Fields must be independently queryable.
- Never log key material, secret-store contents, or raw tokens at any level. Log presence/absence
  (`has_key: bool`) or size (`key_len: usize`) instead.
- Level-gate anything on a per-frame/high-frequency hot path — `tracing`'s macros already
  short-circuit before evaluating field expressions when the level is disabled, so just avoid
  pre-formatting strings before the macro call.
- A logging/tracing failure must never crash the app or propagate as an application error.
