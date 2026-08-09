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

  /**
   * `update_state` delivers a *batch* under `data.state`. Reading `data.type` on it finds
   * nothing, which is how every mid-call membership change was logged as `type=-` -- the
   * exact event this module was built to catch, silently missed. Shape taken from a real
   * message, not invented.
   */
  it("finds membership events inside an update_state batch", () => {
    const line = describeWidgetMessage({
      api: "toWidget",
      action: "update_state",
      data: {
        state: [
          {
            type: "m.room.name",
            state_key: "",
            sender: "@alice:example.org",
            content: { name: "General 1" },
          },
          {
            type: "org.matrix.msc3401.call.member",
            state_key: "_@bob:example.org_DEVICEONE_m.call",
            sender: "@bob:example.org",
            content: { memberships: [{ device_id: "DEVICEONE" }] },
          },
        ],
      },
    });
    expect(line?.text).toContain("MatrixRTC membership");
    expect(line?.text).toContain("JOINED/UPDATED");
    expect(line?.text).toContain("@bob:example.org");
    // The room name rode along in the same batch and is content.
    expect(line?.text).not.toContain("General 1");
  });

  it("reports every membership in a batch, not just the first", () => {
    const line = describeWidgetMessage({
      api: "toWidget",
      action: "update_state",
      data: {
        state: [
          {
            type: "m.rtc.member",
            state_key: "_@bob:example.org_ONE",
            sender: "@bob:example.org",
            content: { memberships: [{}] },
          },
          {
            type: "m.rtc.member",
            state_key: "_@carol:example.org_TWO",
            sender: "@carol:example.org",
            content: {},
          },
        ],
      },
    });
    expect(line?.text).toContain("@bob:example.org");
    expect(line?.text).toContain("@carol:example.org");
    expect(line?.text).toContain("LEFT");
  });

  it("falls back to the plain action line for an update_state with no membership in it", () => {
    const line = describeWidgetMessage({
      api: "toWidget",
      action: "update_state",
      data: { state: [{ type: "m.room.topic", content: { topic: "hello" } }] },
    });
    expect(line?.text).toContain("update_state");
    expect(line?.text).not.toContain("MatrixRTC membership");
    expect(line?.text).not.toContain("hello");
  });

  /**
   * Inbound and outbound to-device carry different shapes. Counting recipients on a
   * delivered event found none and reported `recipients=0` on a key that had arrived
   * perfectly well -- which reads as a failure and is not one.
   */
  it("describes an inbound to-device by its sender, not by a recipient count", () => {
    const line = describeWidgetMessage({
      api: "toWidget",
      action: "send_to_device",
      data: {
        type: "io.element.call.encryption_keys",
        sender: "@bob:example.org",
        encrypted: true,
        content: { keys: "SECRET" },
      },
    });
    expect(line?.text).toContain("from=@bob:example.org");
    expect(line?.text).not.toContain("recipients=0");
    expect(line?.text).not.toContain("SECRET");
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
