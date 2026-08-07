# Tasks: Capture loose ends

**Spec**: [spec.md](spec.md)

- [X] T001 [US1] Report a failed MJPEG decode as a failed decode, not as a short buffer, in `crates/elementium-media/src/pipewire_capture.rs` — the current message describes the raw path and sends a reader to the wrong place
- [ ] T002 [US2] Run a camera for 12 seconds at 30fps and read the capture counters; attribute the missing frames to the camera, the rate limiter, or the consumer
- [ ] T003 [US2] Fix whatever T002 names, or record that the camera simply does not sustain 30fps — which is a perfectly good answer and worth writing down rather than chasing
- [X] T004 [US3] Treat a busy or vanished node as an expected condition rather than an error, since re-enumeration is normal when a camera is switched between machines
- [X] T005 [US1] Check the remaining capture warnings against what the code actually does, since two of the three examined so far were describing the wrong condition
