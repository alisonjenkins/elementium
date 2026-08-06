//! In-memory `tracing` event capture fixture for regression and
//! secret-redaction tests.
//!
//! Built per `specs/002-observability-structured-logging/research.md`'s
//! "Test-capture fixture" decision: a small custom [`tracing_subscriber::Layer`]
//! that records every emitted event into an in-memory `Vec` a test can
//! inspect after running code under [`tracing::subscriber::with_default`].
//!
//! No new external dependency: everything used here (`Layer`, `Registry`,
//! `Subscriber`, `Visit`) is already exposed by `tracing`/`tracing-subscriber`,
//! both already workspace dependencies.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;

/// One captured `tracing` event: its metadata name, level, and all recorded
/// fields (stringified via `Display`/`Debug`, per `tracing`'s `Visit` trait).
///
/// The event's `"..."` message (if any) is stored under the `"message"` key,
/// matching how `tracing`'s JSON formatter represents it.
#[derive(Debug, Clone)]
pub struct CapturedEvent {
    /// The `tracing` metadata name for this event (e.g. `"event <file>:<line>"`).
    pub name: String,
    /// The event's level (`INFO`, `WARN`, `ERROR`, ...).
    pub level: Level,
    /// All recorded fields, keyed by field name, values stringified.
    pub fields: HashMap<String, String>,
}

impl CapturedEvent {
    /// Look up a field's stringified value by name.
    #[must_use]
    pub fn field(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }

    /// The event's `"..."` message field, if any.
    #[must_use]
    pub fn message(&self) -> Option<&str> {
        self.field("message")
    }
}

/// Collects an event's fields into a `HashMap<String, String>`.
struct FieldVisitor<'a> {
    fields: &'a mut HashMap<String, String>,
}

impl Visit for FieldVisitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.fields
            .insert(field.name().to_owned(), format!("{value:?}"));
    }
}

/// The `Layer` implementation: on every event, records it into the shared
/// `events` buffer. Cheap and side-effect-free otherwise (no formatting to
/// stdout, no filtering — that's the test's job via `find_event`/`field`).
#[derive(Clone)]
struct CaptureLayer {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl<S: Subscriber> Layer<S> for CaptureLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut fields = HashMap::new();
        let mut visitor = FieldVisitor {
            fields: &mut fields,
        };
        event.record(&mut visitor);

        let captured = CapturedEvent {
            name: event.metadata().name().to_owned(),
            level: *event.metadata().level(),
            fields,
        };

        if let Ok(mut guard) = self.events.lock() {
            guard.push(captured);
        }
    }
}

/// Test-facing wrapper around the capture fixture.
///
/// Construct fresh per test (no shared/global state, so tests using it run
/// safely in parallel), run the code under test via [`LogCapture::run`], then
/// inspect what was captured via [`LogCapture::events`] / [`LogCapture::find_event`].
#[derive(Clone, Default)]
pub struct LogCapture {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl LogCapture {
    /// Construct a fresh, empty capture fixture.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `f` under a subscriber that records every emitted `tracing` event
    /// into this fixture. Scoped to the closure via
    /// [`tracing::subscriber::with_default`], so it does not affect any
    /// other thread or leak past this call.
    pub fn run<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let layer = CaptureLayer {
            events: Arc::clone(&self.events),
        };
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, f)
    }

    /// Install this fixture as the process-wide global default `tracing` subscriber.
    ///
    /// [`LogCapture::run`]'s `tracing::subscriber::with_default` is thread-local: it can't
    /// see events emitted on other threads/tasks (e.g. a background `tokio::spawn`'d I/O
    /// loop), which most single-function unit tests don't need but a full multi-task
    /// integration test (like a real two-client network round trip) does. Call this once,
    /// at the very start of such a test, instead of `run`. Idempotent: a second call within
    /// the same process is a no-op rather than a panic, since `tracing` only allows setting
    /// the global default once per process.
    pub fn install_global(&self) {
        let layer = CaptureLayer {
            events: Arc::clone(&self.events),
        };
        let subscriber = tracing_subscriber::registry().with(layer);
        let _ = tracing::subscriber::set_global_default(subscriber);
    }

    /// All events captured so far, in emission order.
    #[must_use]
    pub fn events(&self) -> Vec<CapturedEvent> {
        self.events.lock().map(|guard| guard.clone()).unwrap_or_default()
    }

    /// Find the first captured event whose message or metadata name contains
    /// `needle` as a substring.
    #[must_use]
    pub fn find_event(&self, needle: &str) -> Option<CapturedEvent> {
        self.events().into_iter().find(|e| {
            e.message().is_some_and(|m| m.contains(needle)) || e.name.contains(needle)
        })
    }

    /// Assert that `needle` (the exact field value) is present on the event
    /// found by [`find_event`], returning `true` if both the event and the
    /// field/value match.
    #[must_use]
    pub fn assert_field(&self, event_needle: &str, field: &str, expected_value: &str) -> bool {
        self.find_event(event_needle)
            .and_then(|e| e.field(field).map(str::to_owned))
            .is_some_and(|actual| actual == expected_value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Test assertions are meant to panic on failure; expect()/unwrap() with a
    // descriptive message is the idiomatic way to do that in test code.
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn captures_event_name_level_and_fields() {
        let capture = LogCapture::new();
        capture.run(|| {
            tracing::warn!(reason = "no_key", label = "audio", "frame dropped");
        });

        let event = capture
            .find_event("frame dropped")
            .expect("event should have been captured");
        assert_eq!(event.level, Level::WARN);
        assert_eq!(event.field("reason"), Some("no_key"));
        assert_eq!(event.field("label"), Some("audio"));
        assert!(capture.assert_field("frame dropped", "reason", "no_key"));
    }

    #[test]
    fn empty_when_nothing_emitted() {
        let capture = LogCapture::new();
        capture.run(|| {});
        assert!(capture.events().is_empty());
    }

    #[test]
    fn independent_captures_do_not_leak_into_each_other() {
        let a = LogCapture::new();
        let b = LogCapture::new();
        a.run(|| tracing::info!("only in a"));
        b.run(|| tracing::info!("only in b"));
        assert!(a.find_event("only in a").is_some());
        assert!(a.find_event("only in b").is_none());
        assert!(b.find_event("only in b").is_some());
        assert!(b.find_event("only in a").is_none());
    }
}
