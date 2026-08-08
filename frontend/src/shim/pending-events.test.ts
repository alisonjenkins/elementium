/**
 * Events that arrive before a peer connection has registered its handler must be kept.
 *
 * Rust starts forwarding as soon as the connection exists; the shim registers only once the
 * `invoke()` that created it resolves. Whatever lands in that window used to be dropped —
 * and it is the worst possible window to lose, because the earliest events are the ICE
 * candidates and the first connection-state change. A lost candidate costs connectivity
 * that nothing re-announces, and a lost state change is the only notice of a transition
 * that will never repeat.
 */
import { describe, expect, it } from "vitest";

interface Event {
  type: string;
  pcId: string;
}

/** The production hold-and-replay logic, with the registry injected so it can be observed. */
function makeDispatcher(maxHeld: number) {
  const registry = new Map<string, (e: Event) => void>();
  const pending = new Map<string, Event[]>();
  let dropped = 0;

  const dispatch = (event: Event): void => {
    const handler = registry.get(event.pcId);
    if (handler) {
      handler(event);
      return;
    }
    const held = pending.get(event.pcId) ?? [];
    if (held.length < maxHeld) {
      held.push(event);
      pending.set(event.pcId, held);
    } else {
      dropped += 1;
    }
  };

  const register = (pcId: string, handler: (e: Event) => void): void => {
    registry.set(pcId, handler);
    const held = pending.get(pcId);
    if (!held) return;
    pending.delete(pcId);
    for (const event of held) handler(event);
  };

  return { dispatch, register, droppedCount: () => dropped, heldFor: (id: string) => pending.get(id)?.length ?? 0 };
}

describe("events arriving before registration", () => {
  it("delivers what arrived early, in the order it arrived", () => {
    const d = makeDispatcher(64);
    d.dispatch({ type: "iceCandidate", pcId: "pc1" });
    d.dispatch({ type: "connectionStateChange", pcId: "pc1" });

    const seen: string[] = [];
    d.register("pc1", (e) => seen.push(e.type));

    // Order matters: a state change applied before the candidates that caused it would
    // leave the connection describing a state its own inputs contradict.
    expect(seen).toEqual(["iceCandidate", "connectionStateChange"]);
  });

  it("delivers straight through once registered, without queueing", () => {
    const d = makeDispatcher(64);
    const seen: string[] = [];
    d.register("pc1", (e) => seen.push(e.type));

    d.dispatch({ type: "connected", pcId: "pc1" });

    expect(seen).toEqual(["connected"]);
    expect(d.heldFor("pc1")).toBe(0);
  });

  it("does not hold events for one connection against another", () => {
    const d = makeDispatcher(64);
    d.dispatch({ type: "iceCandidate", pcId: "pc1" });

    const seen: string[] = [];
    d.register("pc2", (e) => seen.push(e.type));

    expect(seen).toEqual([]);
    expect(d.heldFor("pc1")).toBe(1);
  });

  it("stops holding once the cap is reached, rather than growing without bound", () => {
    // A connection closed before it finished opening never registers. Without a cap its
    // events would accumulate for the lifetime of the page.
    const d = makeDispatcher(2);
    d.dispatch({ type: "a", pcId: "gone" });
    d.dispatch({ type: "b", pcId: "gone" });
    d.dispatch({ type: "c", pcId: "gone" });

    expect(d.heldFor("gone")).toBe(2);
    expect(d.droppedCount()).toBe(1);
  });
});
