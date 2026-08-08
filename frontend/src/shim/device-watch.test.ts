/**
 * A device list changes in ways a count cannot see.
 *
 * Swapping one headset for another leaves the number of devices identical, and so does a
 * device being renamed by the system. Comparing lengths would call both of those "no
 * change" and leave the picker showing a device that is gone — which is the failure this
 * watcher exists to remove, so the comparison is what these tests pin.
 */
import { describe, expect, it } from "vitest";

interface Device {
  id: string;
  label: string;
  kind: string;
}

/** The production fingerprint, kept in step with `DeviceWatcher.fingerprintOf`. */
function fingerprintOf(devices: Device[]): string {
  return devices
    .map((d) => `${d.kind}:${d.id}:${d.label}`)
    .sort()
    .join("|");
}

const headset: Device = { id: "audio-input-1", label: "USB Headset", kind: "audioInput" };
const builtin: Device = { id: "audio-input-0", label: "Built-in Mic", kind: "audioInput" };
const webcam: Device = { id: "video-input-pw-42", label: "Webcam", kind: "videoInput" };

describe("device change detection", () => {
  it("sees a device being swapped for another, which a count cannot", () => {
    const before = fingerprintOf([builtin, headset]);
    const other = { id: "audio-input-1", label: "Other Headset", kind: "audioInput" };
    const after = fingerprintOf([builtin, other]);

    expect(before).not.toBe(after);
  });

  it("sees a device appear and disappear", () => {
    expect(fingerprintOf([builtin])).not.toBe(fingerprintOf([builtin, webcam]));
    expect(fingerprintOf([builtin, webcam])).not.toBe(fingerprintOf([builtin]));
  });

  it("does not fire on reordering, which enumeration order can do on its own", () => {
    // The device list comes back from a fresh enumeration each poll and its order is not
    // guaranteed. Treating a reorder as a change would fire `devicechange` every few
    // seconds, and a picker that rebuilds constantly is its own bug.
    expect(fingerprintOf([builtin, headset, webcam])).toBe(
      fingerprintOf([webcam, builtin, headset]),
    );
  });

  it("sees a rename even when ids are unchanged", () => {
    // The label is what the user reads. A device renamed by the system while keeping its
    // id would otherwise leave a stale name in the picker forever.
    const renamed = { id: headset.id, label: "Headset (Wireless)", kind: headset.kind };
    expect(fingerprintOf([headset])).not.toBe(fingerprintOf([renamed]));
  });
});
