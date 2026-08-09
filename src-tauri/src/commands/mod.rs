pub mod console;
pub mod e2ee;
pub mod livekit;
pub mod media_devices;
pub mod screen_capture;
pub mod secrets;
pub mod webrtc;

/// Lock a `std::sync::Mutex`, mapping poisoning to a `String` error the way every
/// `#[tauri::command]` in this module needs (IPC results must be `Result<T, String>`).
///
/// Deliberately does not recover poisoned locks (unlike `elementium_webrtc`'s
/// `peer_connection::lock_pc`): a poisoned command-layer mutex means a previous command
/// panicked mid-mutation of shared state, which is worth surfacing as an error to the
/// frontend rather than silently continuing with possibly-inconsistent state.
///
/// `command` names the caller (e.g. `"create_offer"`) and is carried into the log line this
/// emits on poisoning. Every implementor of this crate's poisoned-lock handling used to
/// flatten the `PoisonError` straight to a string with no `tracing` call at all -- a
/// poisoned lock is itself evidence that *something else* already panicked, which is exactly
/// the kind of fault that must not also vanish silently. See Principle I's closing bullet.
pub trait LockExt<T> {
    fn lock_str(&self, command: &str) -> Result<std::sync::MutexGuard<'_, T>, String>;
}

impl<T> LockExt<T> for std::sync::Mutex<T> {
    fn lock_str(&self, command: &str) -> Result<std::sync::MutexGuard<'_, T>, String> {
        self.lock().map_err(|e| {
            tracing::error!(
                command,
                poison_error = %e,
                "shared state lock poisoned; a previous command panicked while holding it"
            );
            "internal error: shared state lock poisoned".to_owned()
        })
    }
}

/// Walk a typed error's `source()` chain into one string, `<-`-joined outer to innermost.
///
/// Factored out so every flattening site builds the same chain the same way, rather than
/// each command re-implementing (or forgetting) the walk.
pub fn error_chain(error: &dyn std::error::Error) -> String {
    let mut chain = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        chain.push_str(" <- ");
        chain.push_str(&cause.to_string());
        source = cause.source();
    }
    chain
}

/// Flatten a typed (`std::error::Error`) `Result` for the IPC boundary, logging the full
/// cause chain exactly once on the way out.
///
/// This is the universal mechanism this module routes every fallible command through: the
/// page can only ever receive a `String` (Tauri serializes the error), so a typed error's
/// chain dies at this boundary unless something records it first. Doing that as an extension
/// method on `Result` -- called at the same `?`-flattening point a command already needs --
/// means forgetting requires *not* calling `?`/`map_err` at all, rather than merely picking
/// the wrong one of several ad hoc flattening idioms. That was the actual failure mode this
/// exists for: an `ok_or("...")` compiles exactly as easily as a call through here, and nothing
/// short of routing everything through one path stops the next command from reintroducing it.
///
/// `id` names the entity the command was acting on (a `pc_id`, `room_id`, track key, ...) for
/// correlating this failure with the rest of that entity's log lines; pass `""` when there is
/// none (e.g. a command with no connection/session argument).
pub trait IpcErr<T> {
    fn ipc_err(self, command: &str, id: &str) -> Result<T, String>;
}

impl<T, E> IpcErr<T> for Result<T, E>
where
    E: std::error::Error,
{
    fn ipc_err(self, command: &str, id: &str) -> Result<T, String> {
        self.map_err(|e| {
            let chain = error_chain(&e);
            tracing::error!(command, id, chain = %chain, "command failed");
            e.to_string()
        })
    }
}

#[cfg(test)]
mod logging_tests {
    use elementium_observability_test::LogCapture;

    use super::{IpcErr, LockExt};

    /// A minimal typed error with one layer of `source()`, standing in for the workspace's
    /// real `thiserror`-derived error enums so this test does not depend on any of them.
    #[derive(Debug)]
    struct Inner;

    impl std::fmt::Display for Inner {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "inner cause")
        }
    }

    impl std::error::Error for Inner {}

    #[derive(Debug)]
    struct Outer(Inner);

    impl std::fmt::Display for Outer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "outer failure")
        }
    }

    impl std::error::Error for Outer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    /// Pins the bug this whole change exists to close (see the constitution's Principle I,
    /// and the incident it cites): a fallible command's `Err` reaching the page with nothing
    /// recorded anywhere. `IpcErr::ipc_err` is the mechanism every typed-error call site in
    /// this crate now routes through instead of a bare `map_err(|e| e.to_string())`; this
    /// shows it actually logs -- exactly once, at `error`, with the full cause chain -- not
    /// merely that it still returns a string for the page.
    #[test]
    fn ipc_err_logs_the_full_cause_chain_exactly_once() {
        let capture = LogCapture::new();
        let result: Result<(), Outer> = Err(Outer(Inner));

        let flattened = capture.run(|| result.ipc_err("test_command", "widget-1"));

        assert_eq!(flattened, Err("outer failure".to_owned()));

        let error_events: Vec<_> = capture
            .events()
            .into_iter()
            .filter(|e| e.level == tracing::Level::ERROR)
            .collect();
        assert_eq!(
            error_events.len(),
            1,
            "expected exactly one ERROR event, got {error_events:?}"
        );
        let event = error_events.first();
        assert_eq!(event.and_then(|e| e.field("command")), Some("test_command"));
        assert_eq!(event.and_then(|e| e.field("id")), Some("widget-1"));
        let chain = event.and_then(|e| e.field("chain")).unwrap_or_default();
        assert!(chain.contains("outer failure"), "chain was {chain:?}");
        assert!(chain.contains("inner cause"), "chain was {chain:?}");
    }

    /// Pins the other named fix: [`LockExt::lock_str`] used to flatten a poisoned lock to a
    /// string with no `tracing` call at all, so the one condition that proves *something
    /// else* already panicked left no trace of its own -- inherited by every one of this
    /// crate's ~15 call sites at once, which is exactly why it was worth fixing here rather
    /// than at each of them. Also pins that the calling command's name reaches the log, since
    /// a poisoned-lock line with no command name is nearly as hard to act on as none at all.
    #[test]
    // `panic` is how a mutex is poisoned; there is no other way to reach the condition
    // under test. `unwrap` on the lock is the same story.
    #[allow(clippy::unwrap_used, clippy::panic)]
    fn poisoned_lock_logs_exactly_once_with_the_calling_command() {
        let mutex = std::sync::Mutex::new(0_i32);
        // Poisoning requires an actual unwind while the guard is alive; a scoped thread
        // that panics holding it is the only way to reach the condition under test.
        let _ = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let _held = mutex.lock().unwrap();
                    panic!("poison the mutex on purpose, for the test");
                })
                .join()
        });
        assert!(mutex.is_poisoned(), "test setup failed: mutex did not poison");

        let capture = LogCapture::new();
        // Reduced to a bool immediately: holding the `Result` keeps a `MutexGuard` alive
        // for the rest of the test, which clippy objects to and which would be a real
        // deadlock hazard in anything less trivial.
        let refused = capture
            .run(|| mutex.lock_str("set_capture_muted"))
            .is_err();

        assert!(refused);
        let error_events: Vec<_> = capture
            .events()
            .into_iter()
            .filter(|e| e.level == tracing::Level::ERROR)
            .collect();
        assert_eq!(
            error_events.len(),
            1,
            "expected exactly one ERROR event, got {error_events:?}"
        );
        assert_eq!(
            error_events.first().and_then(|e| e.field("command")),
            Some("set_capture_muted")
        );
    }
}

// A `Result<T, String>` counterpart to `IpcErr` (for sites whose failure is already a bare
// `String` rather than a typed error) was drafted alongside it and then deleted: every site
// in this crate that used to build such a `String` ad hoc -- "peer connection not found",
// "not a publishable track", "E2EE not initialized" -- turned out to want its own one-line
// log (naming the specific field worth correlating on: `pc_id`, `kind`/`source`, ...) more
// than a generic wrapper could give it, so each became a small named helper
// (`pc_not_found`, `track_key`, `e2ee_not_initialized`, `get_room`) instead. `IpcErr` and
// `LockExt` remain the two general-purpose mechanisms; a `Result<T, String>` site that
// genuinely has nothing entity-specific to say should reach for a helper shaped like those,
// not reintroduce a bare `ok_or`/`format!` that logs nothing.
