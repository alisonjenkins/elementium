use tauri::command;

/// Receive JS console messages and log them on the Rust side.
/// This lets us see all browser console output (including from iframes)
/// in the log stream.
#[command]
pub async fn console_log(level: String, args: Vec<String>) {
    let msg = redact_secrets(&args.join(" "));
    match level.as_str() {
        "error" => tracing::error!(target: "js_console", "{msg}"),
        "warn" => tracing::warn!(target: "js_console", "{msg}"),
        "debug" => tracing::debug!(target: "js_console", "{msg}"),
        _ => tracing::info!(target: "js_console", "{msg}"),
    }
}

/// Remove media-encryption key material from a console line before it is logged.
///
/// Element Call's widget transport logs the `io.element.call.encryption_keys` payloads it
/// sends, base64 key and all. Forwarding console output verbatim put those on stderr,
/// which was transient; now that the same events are written to a file, every key from
/// every call would be persisted to disk in plaintext, outliving the call it belongs to by
/// as long as the log is kept. A local file is not a safe place for a key that the rest of
/// this codebase is careful to keep in `Zeroizing` and never log.
///
/// Matches the JSON shape rather than any base64-looking string: precision matters here,
/// because a redactor that fires too widely quietly destroys the diagnostic value of the
/// log, and one that fires too narrowly is worse than none at all.
#[must_use]
pub fn redact_secrets(line: &str) -> String {
    const KEY_FIELD: &str = "\"key\":\"";
    let mut out = String::with_capacity(line.len());
    let mut rest = line;

    while let Some(start) = rest.find(KEY_FIELD) {
        let (before, after) = rest.split_at(start.saturating_add(KEY_FIELD.len()));
        out.push_str(before);
        out.push_str("<redacted>");
        // An unterminated value means the line is truncated mid-key; drop the remainder
        // rather than emit a partial key.
        let Some(end) = after.find('"') else {
            return out;
        };
        rest = after.get(end..).unwrap_or("");
    }

    out.push_str(rest);
    out
}

#[cfg(test)]
mod redaction_tests {
    use super::redact_secrets;

    /// The exact shape found in a real log: Element Call's widget transport logging the
    /// encryption-key payload it is about to send.
    #[test]
    fn media_keys_are_removed_from_widget_traffic() {
        let line = r#"{"type":"io.element.call.encryption_keys","messages":{"@a:b.com":{"DEV":{"keys":{"index":1,"key":"DAlNV2nCv56RhJ8PxpQMcw=="},"room_id":"!r:b.com"}}}}"#;
        let redacted = redact_secrets(line);

        assert!(
            !redacted.contains("DAlNV2nCv56RhJ8PxpQMcw=="),
            "the key must not survive: {redacted}"
        );
        assert!(
            redacted.contains("<redacted>"),
            "and the removal must be visible, not silent: {redacted}"
        );
        assert!(
            redacted.contains("\"index\":1")
                && redacted.contains("io.element.call.encryption_keys"),
            "everything that makes the line useful must remain: {redacted}"
        );
    }

    /// Several keys in one line -- the real payload carries one per recipient device.
    #[test]
    fn every_key_in_a_line_is_removed() {
        let line = r#"{"a":{"key":"AAAA"},"b":{"key":"BBBB"},"c":{"key":"CCCC"}}"#;
        let redacted = redact_secrets(line);
        for secret in ["AAAA", "BBBB", "CCCC"] {
            assert!(!redacted.contains(secret), "{secret} survived: {redacted}");
        }
    }

    /// A line truncated mid-key must not leak the part that did arrive.
    #[test]
    fn a_truncated_key_is_not_partially_emitted() {
        let redacted = redact_secrets(r#"{"keys":{"key":"DAlNV2nCv5"#);
        assert!(!redacted.contains("DAlNV2nCv5"), "{redacted}");
    }

    /// Ordinary console output must be untouched: a redactor that mangles normal lines
    /// costs more than it saves.
    #[test]
    fn ordinary_lines_pass_through_unchanged() {
        let line = "[Elementium] createOffer: pcId=pc-123 hasVideo=true";
        assert_eq!(redact_secrets(line), line);
    }

    /// A field merely named "key" elsewhere is still redacted, deliberately: guessing
    /// which "key" is a secret from context is not something a log filter can do safely.
    #[test]
    fn the_match_is_on_shape_not_on_guessing_intent() {
        let redacted = redact_secrets(r#"{"key":"not-actually-secret"}"#);
        assert!(!redacted.contains("not-actually-secret"), "{redacted}");
    }
}
