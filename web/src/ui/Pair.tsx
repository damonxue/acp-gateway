// Pairing: scan the QR code the desktop shows, or paste it.
//
// The camera path uses the platform BarcodeDetector when available and falls
// back to jsQR, because a phone without a working scanner must still be able to
// pair — hence the paste box is always present, not a hidden "advanced" option.

import { useEffect, useRef, useState } from "preact/hooks";
import { pair, parseOffer } from "../api/gateway";
import type { StoredDevice } from "../api/store";

interface Props {
  onPaired(device: StoredDevice): void;
}

type BarcodeDetectorLike = {
  detect(source: CanvasImageSource): Promise<{ rawValue: string }[]>;
};

export function Pair({ onPaired }: Props) {
  const [raw, setRaw] = useState("");
  const [name, setName] = useState(defaultDeviceName());
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [scanning, setScanning] = useState(false);
  const video = useRef<HTMLVideoElement | null>(null);

  const submit = async (payload: string) => {
    setBusy(true);
    setError(null);
    try {
      const device = await pair(parseOffer(payload), name.trim() || "Phone");
      onPaired(device);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusy(false);
    }
  };

  useEffect(() => {
    if (!scanning) return;
    let stream: MediaStream | null = null;
    let frame = 0;
    let cancelled = false;

    const scan = async () => {
      try {
        stream = await navigator.mediaDevices.getUserMedia({
          video: { facingMode: "environment" },
        });
        if (cancelled) return;
        const element = video.current;
        if (!element) return;
        element.srcObject = stream;
        await element.play();

        const detector = await makeDetector();
        const canvas = document.createElement("canvas");
        const context = canvas.getContext("2d", { willReadFrequently: true });

        const tick = async () => {
          if (cancelled || !context) return;
          if (element.videoWidth > 0) {
            canvas.width = element.videoWidth;
            canvas.height = element.videoHeight;
            context.drawImage(element, 0, 0);
            const found = await detector(canvas, context);
            if (found) {
              setScanning(false);
              void submit(found);
              return;
            }
          }
          frame = requestAnimationFrame(() => void tick());
        };
        await tick();
      } catch (cause) {
        setScanning(false);
        setError(
          cause instanceof Error
            ? `camera unavailable: ${cause.message}`
            : "camera unavailable",
        );
      }
    };

    void scan();
    return () => {
      cancelled = true;
      cancelAnimationFrame(frame);
      stream?.getTracks().forEach((track) => track.stop());
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [scanning]);

  return (
    <div class="pair">
      <h1>Agent Gateway</h1>
      <p class="muted">
        Run <code>agent-gateway pair</code> on your computer (or open the desktop app) and
        scan the code it shows.
      </p>

      {scanning ? (
        <div class="scanner">
          <video ref={video} playsInline muted />
          <button type="button" class="secondary" onClick={() => setScanning(false)}>
            Stop camera
          </button>
        </div>
      ) : (
        <button type="button" onClick={() => setScanning(true)} disabled={busy}>
          Scan QR code
        </button>
      )}

      <label>
        Device name
        <input
          value={name}
          onInput={(event) => setName((event.target as HTMLInputElement).value)}
          placeholder="iPhone"
        />
      </label>

      <label>
        …or paste the pairing JSON
        <textarea
          value={raw}
          rows={4}
          spellcheck={false}
          onInput={(event) => setRaw((event.target as HTMLTextAreaElement).value)}
          placeholder='{"machine_id":"machine_…","pairing_code":"834921",…}'
        />
      </label>
      <button type="button" disabled={busy || raw.trim() === ""} onClick={() => void submit(raw)}>
        {busy ? "Pairing…" : "Pair"}
      </button>

      {error !== null && <p class="error">{error}</p>}
      <p class="muted small">
        Pairing codes are single-use and expire in minutes. Your device key is generated
        here and never leaves this phone.
      </p>
    </div>
  );
}

/** Prefer the platform scanner; fall back to jsQR where it is missing. */
async function makeDetector(): Promise<
  (canvas: HTMLCanvasElement, context: CanvasRenderingContext2D) => Promise<string | null>
> {
  const native = (globalThis as { BarcodeDetector?: new (options: { formats: string[] }) => BarcodeDetectorLike })
    .BarcodeDetector;
  if (native) {
    const detector = new native({ formats: ["qr_code"] });
    return async (canvas) => {
      const codes = await detector.detect(canvas);
      return codes[0]?.rawValue ?? null;
    };
  }
  const { default: jsQR } = await import("jsqr");
  return async (canvas, context) => {
    const image = context.getImageData(0, 0, canvas.width, canvas.height);
    const code = jsQR(image.data, image.width, image.height);
    return code?.data ?? null;
  };
}

function defaultDeviceName(): string {
  const agent = navigator.userAgent;
  if (/iPhone/.test(agent)) return "iPhone";
  if (/iPad/.test(agent)) return "iPad";
  if (/Android/.test(agent)) return "Android";
  return "Browser";
}
