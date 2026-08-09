import { describe, expect, it } from "vitest";

import { describeWidgetMessage } from "./widget-api-log";

/**
 * This module exists to answer one question from a log -- did Element Call see the
 * membership change, and did it distribute a key -- and it reads the two messages that
 * carry the most sensitive payload in the application to do it. A `send_to_device` for
 * `m.room.encrypted` contains the frame encryption keys. So the hard constraint is the same
 * one `e2ee-worker-messages.test.ts` pins for the key bridge: kinds, types, identities and
 * counts are fine to log; content never is.
 */
describe("describeWidgetMessage", () => {
  /** A realistically-shaped to-device send, with material that must not appear in a log. */
  const sendToDevice = {
    api: "fromWidget",
    action: "send_to_device",
    data: {
      type: "m.room.encrypted",
      encrypted: true,
      messages: {
        "@alice:example.org": {
          DEVICEONE: { ciphertext: "SUPER-SECRET-KEY-MATERIAL", algorithm: "m.olm.v1" },
          DEVICETWO: { ciphertext: "ALSO-SECRET", algorithm: "m.olm.v1" },
        },
        "@bob:example.org": {
          DEVICETHREE: { ciphertext: "THIRD-SECRET", algorithm: "m.olm.v1" },
        },
      },
    },
  };

  it("reports a to-device send by type and recipient count", () => {
    const line = describeWidgetMessage(sendToDevice);
    expect(line?.text).toContain("send_to_device");
    expect(line?.text).toContain("type=m.room.encrypted");
    // Two devices for alice, one for bob.
    expect(line?.text).toContain("recipients=3");
    expect(line?.oncePerAction).toBe(false);
  });

  it("never puts to-device content in the line", () => {
    const text = describeWidgetMessage(sendToDevice)?.text ?? "";
    expect(text).not.toContain("SUPER-SECRET-KEY-MATERIAL");
    expect(text).not.toContain("ALSO-SECRET");
    expect(text).not.toContain("THIRD-SECRET");
    expect(text).not.toContain("ciphertext");
  });

  it("counts zero recipients rather than throwing on a malformed messages map", () => {
    for (const messages of [undefined, null, "nonsense", 42, []]) {
      const line = describeWidgetMessage({
        api: "fromWidget",
        action: "send_to_device",
        data: { type: "m.room.encrypted", messages },
      });
      expect(line?.text).toContain("recipients=0");
    }
  });

  /**
   * A membership event's content is an array of per-device memberships, so leaving is not a
   * distinct event type -- it is that array becoming empty. A reader who does not know that
   * sees a join at the moment someone leaves, which is precisely backwards for attributing a
   * key rotation.
   */
  it("distinguishes a join from a leave by device count, not by event type", () => {
    const joined = describeWidgetMessage({
      api: "toWidget",
      action: "update_state",
      data: {
        type: "org.matrix.msc3401.call.member",
        state_key: "_@alice:example.org_DEVICEONE",
        sender: "@alice:example.org",
        content: { memberships: [{ device_id: "DEVICEONE" }] },
      },
    });
    expect(joined?.text).toContain("JOINED/UPDATED");
    expect(joined?.text).toContain("devices=1");

    const left = describeWidgetMessage({
      api: "toWidget",
      action: "update_state",
      data: {
        type: "org.matrix.msc3401.call.member",
        state_key: "_@alice:example.org_DEVICEONE",
        sender: "@alice:example.org",
        content: {},
      },
    });
    expect(left?.text).toContain("LEFT");
    expect(left?.text).toContain("devices=0");
  });

  it("recognises every spelling of the membership event this stack has used", () => {
    for (const type of ["org.matrix.msc3401.call.member", "m.call.member", "m.rtc.member"]) {
      const line = describeWidgetMessage({
        api: "toWidget",
        action: "update_state",
        data: { type, content: { memberships: [{}] } },
      });
      expect(line?.text, type).toContain("MatrixRTC membership");
    }
  });

  it("marks ordinary widget chatter as once-per-action so it cannot bury the rest", () => {
    const line = describeWidgetMessage({ api: "toWidget", action: "theme_change", data: {} });
    expect(line?.oncePerAction).toBe(true);
    expect(line?.key).toBe("toWidget:theme_change");
  });

  it("keeps a state update that is not a membership event, but does not call it one", () => {
    const line = describeWidgetMessage({
      api: "toWidget",
      action: "update_state",
      data: { type: "m.room.topic", content: { topic: "hello" } },
    });
    expect(line?.text).toContain("update_state");
    expect(line?.text).not.toContain("MatrixRTC membership");
    // The topic is content, and content is not logged.
    expect(line?.text).not.toContain("hello");
  });

  it("ignores anything that is not widget API traffic", () => {
    expect(describeWidgetMessage({ action: "send_to_device", data: {} })).toBeNull();
    expect(describeWidgetMessage({ api: "somethingElse", action: "send_to_device" })).toBeNull();
    expect(describeWidgetMessage({})).toBeNull();
  });

  it("does not throw on a message with no action or no data", () => {
    expect(() => describeWidgetMessage({ api: "toWidget" })).not.toThrow();
    expect(() => describeWidgetMessage({ api: "toWidget", action: 42 })).not.toThrow();
    expect(describeWidgetMessage({ api: "toWidget" })?.text).toContain("?");
  });
});
