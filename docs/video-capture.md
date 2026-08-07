# Video capture: camera and screen

## Why V4L2 cannot work here

`/dev/video3` (the webcam) is held open by the `pipewire` daemon, and `/dev/video11`
by OBS. Opening the device node succeeds; *setting a format* is what returns
`EBUSY`. That distinction matters, because "cannot open the camera" invites fixing
the open call, and there is nothing to fix there.

Run the probe to see the current state of the machine:

```bash
cargo run -p elementium-media --example camera_probe
```

On this machine it reports:

```
/dev/video3   EBUSY setting format   the real camera, held by pipewire
/dev/video11  EBUSY opening stream   OBS virtual camera, held by OBS
/dev/video4   CameraFormat failure   a UVC metadata node, not a camera
/dev/video10  CameraFormat failure   likewise
```

The last two are worth knowing about: a UVC camera exposes a second device node
for metadata that enumerates as a camera but has no usable format. Any code that
takes "the first enumerated camera" will sometimes take that one.

## PipeWire

`VideoSource::start` tries PipeWire first and falls back to V4L2. That order is
deliberate — a PipeWire-managed camera cannot be opened directly, while a
directly-openable camera is almost always also visible through PipeWire, so the
reverse order would work by luck on some machines and fail on others.

Measured on this machine: **1280x720 at 60.1 fps**, 3,686,400 bytes per RGBA frame
(exactly `width * height * 4`).

### Three things that were needed to get frames

Each of these produces a stream that connects, reports `Streaming`, and delivers
nothing — so none of them is visible without measuring frames.

1. **The requested format must include size and framerate.** Without them the
   source fixates a format whose pixel layout parses as `Unknown`, and every frame
   is dropped for want of a conversion.
2. **`VideoInfoRaw` only describes raw video.** A compressed stream reports an
   `Unknown` pixel format while still parsing the size, which is indistinguishable
   from "a raw format we do not support" when it is really "not raw at all". The
   `MediaSubtype` is read from the pod directly to tell them apart.
3. **The camera only offers MJPEG at 1280x720, and PipeWire does not transcode.**
   A client offering only RGB layouts negotiates something the camera cannot fill
   and receives nothing. MJPEG is decoded; YUY2 is handled too, since that is what
   UVC cameras offer at lower resolutions.

`AUTOCONNECT` is required. Without it the stream reaches `Paused` and no link is
ever created.

### Row stride

PipeWire aligns rows, so the source stride is frequently larger than
`width * bytes_per_pixel`. Ignoring it shears the image progressively down the
frame — a diagonal skew that is easy to recognise once seen and easy to miss in a
still. A buffer too small for the claimed geometry is refused rather than
converted into a partly-garbage frame.

## Screen capture

Wayland has no way to read the screen directly; the compositor does not expose one,
by design. The route is the `org.freedesktop.portal.ScreenCast` portal, which shows
a picker and hands back a PipeWire node id — at which point it is the same capture
path as the camera. That is why fixing the camera unblocked screen sharing.

`sources()` returns an empty list on purpose: the compositor will not tell an
application what windows exist, so any list we produced would be empty or a lie.

**Not yet verified end to end** — it needs someone to click the portal picker.

## Gaps

- The OBS virtual camera (node 161) negotiates no format we can decode: it reports
  "no more input formats" against our list. It is skipped, and a real camera is
  used instead, but it would be nice to support.
- `camera_probe` has no assertion on decoded JPEG dimensions; the check that
  decoded bytes equal `width * height * 4` is done by eye in its output.
