/**
 * Secret storage shim for Elementium.
 *
 * Intercepts localStorage writes to sensitive Matrix session keys and
 * mirrors them to the native keyring via Tauri IPC. Reads are served
 * from real localStorage (pre-populated by the Rust initialization_script).
 *
 * Uses Storage.prototype method patching instead of a Proxy so that
 * window.localStorage remains the real Storage object. This avoids
 * WebKit internal-slot checks that break with Proxy-wrapped Storage
 * (e.g. .length getter, instanceof, and webpack module evaluation order).
 */

const SENSITIVE_KEYS = new Set([
  "mx_access_token",
  "mx_pickle_key",
  "mx_has_pickle_key",
  "mx_user_id",
  "mx_device_id",
  "mx_hs_url",
  "mx_is_guest",
]);

/**
 * Lazily invoke a Tauri command. Uses __TAURI_INTERNALS__ directly
 * to avoid bundling @tauri-apps/api/core into the IIFE (which can
 * cause TDZ issues with webpack chunk evaluation order in Element Web).
 */
function tauriInvoke(cmd: string, args: Record<string, unknown>): void {
  try {
    const internals = (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ as
      | { invoke: (cmd: string, args: Record<string, unknown>) => Promise<unknown> }
      | undefined;
    if (internals) {
      internals.invoke(cmd, args).catch((err: unknown) => {
        console.warn(`[Elementium] Secret store command "${cmd}" failed:`, err);
      });
    }
  } catch {
    // Silently ignore — keyring not available
  }
}

/**
 * Patch Storage.prototype methods to mirror sensitive key writes to the
 * native secret store. Must be called before any other shims or scripts.
 */
export function setupSecretStorageShim(): void {
  // If we're not in Tauri, bail out
  if (!(window as unknown as Record<string, unknown>).__TAURI_INTERNALS__) {
    return;
  }

  const origSetItem = Storage.prototype.setItem;
  const origRemoveItem = Storage.prototype.removeItem;
  const origClear = Storage.prototype.clear;

  Storage.prototype.setItem = function (key: string, value: string): void {
    origSetItem.call(this, key, value);

    // Only mirror localStorage, not sessionStorage
    if (this === window.localStorage && SENSITIVE_KEYS.has(key)) {
      tauriInvoke("secret_set", { key, value });
    }
  };

  Storage.prototype.removeItem = function (key: string): void {
    origRemoveItem.call(this, key);

    if (this === window.localStorage && SENSITIVE_KEYS.has(key)) {
      tauriInvoke("secret_delete", { key });
    }
  };

  Storage.prototype.clear = function (): void {
    const isLocalStorage = this === window.localStorage;
    origClear.call(this);

    if (isLocalStorage) {
      for (const key of SENSITIVE_KEYS) {
        tauriInvoke("secret_delete", { key });
      }
    }
  };

  // Log status
  const w = window as unknown as Record<string, unknown>;
  if (w.__elementium_needs_secret_setup) {
    console.warn(
      "[Elementium] No OS keyring detected — session secrets are stored in localStorage only. " +
      "Consider starting a Secret Service daemon (e.g. gnome-keyring) for encrypted storage."
    );
  } else if (w.__elementium_secrets_loaded) {
    console.log("[Elementium] Session secrets loaded from OS keyring");
  }
}
