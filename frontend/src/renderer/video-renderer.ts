/**
 * Video renderer — fetches decoded RGBA frames from the Rust backend
 * via the custom `elementium://` protocol and renders them to a canvas.
 */

/**
 * Renders video frames for a remote track onto a canvas element.
 */
export class VideoRenderer {
  private canvas: HTMLCanvasElement;
  private ctx: CanvasRenderingContext2D;
  private trackId: string;
  private running = false;
  private rafId: number | null = null;

  constructor(canvas: HTMLCanvasElement, trackId: string) {
    this.canvas = canvas;
    this.ctx = canvas.getContext("2d")!;
    this.trackId = trackId;
  }

  /** Start the render loop. */
  start(): void {
    if (this.running) return;
    this.running = true;
    this.renderLoop();
  }

  /** Stop the render loop. */
  stop(): void {
    this.running = false;
    if (this.rafId !== null) {
      cancelAnimationFrame(this.rafId);
      this.rafId = null;
    }
  }

  private async renderLoop(): Promise<void> {
    if (!this.running) return;

    try {
      const resp = await fetch(`elementium://localhost/video-frame/${this.trackId}`);
      if (resp.ok) {
        const width = parseInt(resp.headers.get("X-Frame-Width") || "0", 10);
        const height = parseInt(resp.headers.get("X-Frame-Height") || "0", 10);

        if (width > 0 && height > 0) {
          // Resize canvas if needed
          if (this.canvas.width !== width || this.canvas.height !== height) {
            this.canvas.width = width;
            this.canvas.height = height;
          }

          const buf = await resp.arrayBuffer();
          const imageData = new ImageData(
            new Uint8ClampedArray(buf),
            width,
            height,
          );
          this.ctx.putImageData(imageData, 0, 0);
        }
      }
    } catch (e) {
      // Frame fetch failed — skip this frame
    }

    this.rafId = requestAnimationFrame(() => this.renderLoop());
  }
}
