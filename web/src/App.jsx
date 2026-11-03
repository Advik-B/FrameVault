import { useCallback, useEffect, useRef, useState } from "react";
import * as P from "purrimer";

import {
  CODECS,
  canPlayHevc,
  decodeVideo,
  encodeVideo,
  loadCodec,
  loadFFmpeg,
  planEncode,
} from "./pipeline.js";


// Purrimer's greyscale pair only: ink on paper by day, chalk on black by night.
const THEMES = [
  { id: "blanc", label: "Blanc", icon: "sun" },
  { id: "noir", label: "Noir", icon: "moon" },
];

// Above this, encoding in a browser tab gets slow enough to warn about.
const LARGE_FILE_BYTES = 256 * 1024;

// ---- formatting ----

const nf = new Intl.NumberFormat("en");

function formatBytes(n) {
  if (n < 1024) return `${nf.format(n)} B`;
  const units = ["KB", "MB", "GB"];
  let v = n / 1024;
  let u = 0;
  while (v >= 1024 && u < units.length - 1) {
    v /= 1024;
    u++;
  }
  return `${v.toFixed(v < 10 ? 1 : 0)} ${units[u]}`;
}

function formatDuration(sec) {
  if (sec < 60) return `${sec.toFixed(sec < 10 ? 1 : 0)} s`;
  const m = Math.floor(sec / 60);
  const s = Math.round(sec % 60);
  return s ? `${m} min ${s} s` : `${m} min`;
}

function download(url, filename) {
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  a.click();
}

function baseName(name) {
  return name.replace(/\.[^.]+$/, "") || name;
}

function errorMessage(e) {
  return e?.message ?? String(e);
}

// ---- shared pieces ----

function DropZone({ accept, file, onFile, disabled, prompt }) {
  const input = useRef(null);
  const [over, setOver] = useState(false);

  const take = (files) => {
    if (!disabled && files?.length) onFile(files[0]);
  };

  return (
    <div
      className={`fv-drop${over ? " is-over" : ""}${disabled ? " is-disabled" : ""}`}
      onDragOver={(e) => {
        e.preventDefault();
        if (!disabled) setOver(true);
      }}
      onDragLeave={() => setOver(false)}
      onDrop={(e) => {
        e.preventDefault();
        setOver(false);
        take(e.dataTransfer.files);
      }}
    >
      <input
        ref={input}
        type="file"
        accept={accept}
        hidden
        onChange={(e) => {
          take(e.target.files);
          e.target.value = "";
        }}
      />
      <P.Icon name={file ? "file" : "upload"} size={24} className="fv-drop-icon" />
      {file ? (
        <div className="fv-drop-text">
          <span className="fv-drop-name">{file.name}</span>
          <span className="text-body-small fg-muted">{formatBytes(file.size)}</span>
        </div>
      ) : (
        <div className="fv-drop-text">
          <span>{prompt}</span>
          <span className="text-body-small fg-muted">Drop it here, or choose one.</span>
        </div>
      )}
      <P.Button size="small" disabled={disabled} onClick={() => input.current?.click()}>
        {file ? "Change file" : "Choose file"}
      </P.Button>
    </div>
  );
}

function Stat({ label, value }) {
  return (
    <div className="fv-stat">
      <span className="text-body-small fg-muted">{label}</span>
      <span className="text-stat-small">{value}</span>
    </div>
  );
}

function Progress({ label, value }) {
  return (
    <div className="fv-progress" aria-live="polite">
      <P.ProgressBar aria-label={label} value={value} />
      <span className="text-body-small fg-muted">{label}</span>
    </div>
  );
}

function useObjectURL(blob) {
  const [url, setUrl] = useState(null);
  useEffect(() => {
    if (!blob) return setUrl(null);
    const u = URL.createObjectURL(blob);
    setUrl(u);
    return () => URL.revokeObjectURL(u);
  }, [blob]);
  return url;
}

// ---- encode ----

function EncodePanel({ engineReady }) {
  const [file, setFile] = useState(null);
  const [plan, setPlan] = useState(null);
  const [codec, setCodec] = useState("h264");
  const [state, setState] = useState("idle"); // idle | planning | encoding | done | error
  const [progress, setProgress] = useState(null);
  const [result, setResult] = useState(null);
  const [error, setError] = useState(null);
  const abort = useRef(null);
  const videoURL = useObjectURL(result?.blob);

  const choose = async (f) => {
    setFile(f);
    setPlan(null);
    setResult(null);
    setError(null);
    setState("planning");
    try {
      const enc = await planEncode(f);
      setPlan({
        totalFrames: enc.totalFrames,
        durationSec: enc.durationSec,
        eccBytes: enc.eccBytes,
        sha256: enc.sha256,
      });
      enc.free();
      setState("idle");
    } catch (e) {
      setError(errorMessage(e));
      setState("error");
    }
  };

  const start = async () => {
    const controller = new AbortController();
    abort.current = controller;
    setResult(null);
    setError(null);
    setState("encoding");
    setProgress({ done: 0, total: plan.totalFrames, phase: "encode" });
    const t0 = performance.now();
    let enc;
    try {
      enc = await planEncode(file);
      const blob = await encodeVideo(enc, codec, {
        signal: controller.signal,
        onProgress: (p) => setProgress({ ...p, fps: p.done / ((performance.now() - t0) / 1000) }),
      });
      setResult({ blob, codec, seconds: (performance.now() - t0) / 1000 });
      setState("done");
    } catch (e) {
      if (controller.signal.aborted) {
        setState("idle");
      } else {
        setError(errorMessage(e));
        setState("error");
      }
    } finally {
      enc?.free();
      abort.current = null;
    }
  };

  const busy = state === "planning" || state === "encoding";
  const pct = progress ? (progress.done / progress.total) * 100 : 0;

  return (
    <P.Card title="Encode a file" headerVisual="upload">
      <div className="fv-panel">
        <DropZone
          file={file}
          onFile={choose}
          disabled={busy}
          prompt="Choose any file to turn into a video"
        />

        {plan && (
          <>
            <div className="fv-stats">
              <Stat label="Frames" value={nf.format(plan.totalFrames)} />
              <Stat label="Video length" value={formatDuration(plan.durationSec)} />
              <Stat label="With error correction" value={formatBytes(plan.eccBytes)} />
            </div>
            {file.size > LARGE_FILE_BYTES && (
              <P.Banner variant="warning" title="This one will take a while">
                {`It needs ${nf.format(plan.totalFrames)} frames of 1080p video. Encoding in a browser tab is much slower than the CLI, so expect a wait.`}
              </P.Banner>
            )}
            <div className="fv-row">
              <P.SegmentedControl
                aria-label="Video codec"
                size="small"
                items={Object.values(CODECS).map((c) => c.label)}
                selectedIndex={Object.keys(CODECS).indexOf(codec)}
                onChange={(i) => !busy && setCodec(Object.keys(CODECS)[i])}
              />
              <span className="text-body-small fg-muted">
                {codec === "h264"
                  ? "Fast, and plays everywhere."
                  : "For HEVC workflows. Slower, and not every browser can preview it."}
              </span>
            </div>
          </>
        )}

        {state === "encoding" && progress && (
          <Progress
            value={progress.phase === "mux" ? 100 : pct}
            label={
              progress.phase === "mux"
                ? "Joining the video parts…"
                : `Encoding frame ${nf.format(progress.done)} of ${nf.format(progress.total)}` +
                  (progress.done ? ` · ${progress.fps.toFixed(1)} frames a second` : "")
            }
          />
        )}

        {state === "error" && (
          <P.Banner variant="critical" title="We couldn't encode this file">
            {`${error} Try again, or use the CLI for this one.`}
          </P.Banner>
        )}

        {result && videoURL && (
          <div className="fv-result">
            {result.codec === "h265" && !canPlayHevc() ? (
              <P.Banner variant="info">
                Your browser can't preview H.265, but the file is fine.
              </P.Banner>
            ) : (
              <video className="fv-video" src={videoURL} controls muted playsInline />
            )}
            <div className="fv-stats">
              <Stat label="MP4 size" value={formatBytes(result.blob.size)} />
              <Stat label="Took" value={formatDuration(result.seconds)} />
            </div>
          </div>
        )}

        <div className="fv-actions">
          {state === "encoding" ? (
            <P.Button leadingVisual="stop" onClick={() => abort.current?.abort()}>
              Stop
            </P.Button>
          ) : result && videoURL ? (
            <>
              <P.Button
                variant="primary"
                leadingVisual="download"
                onClick={() => download(videoURL, `${baseName(file.name)}.mp4`)}
              >
                Download video
              </P.Button>
              <P.Button variant="invisible" leadingVisual="sync" onClick={start}>
                Encode again
              </P.Button>
            </>
          ) : (
            <P.Button
              variant="primary"
              leadingVisual="play"
              disabled={!plan || !engineReady || busy}
              onClick={start}
            >
              Encode video
            </P.Button>
          )}
        </div>
      </div>
    </P.Card>
  );
}

// ---- decode ----

function DecodePanel({ engineReady }) {
  const [file, setFile] = useState(null);
  const [state, setState] = useState("idle"); // idle | decoding | done | error
  const [progress, setProgress] = useState(null);
  const [result, setResult] = useState(null);
  const [error, setError] = useState(null);
  const abort = useRef(null);
  const fileURL = useObjectURL(result?.blob);

  const choose = (f) => {
    setFile(f);
    setResult(null);
    setError(null);
    setState("idle");
  };

  const start = async () => {
    const controller = new AbortController();
    abort.current = controller;
    setResult(null);
    setError(null);
    setState("decoding");
    setProgress({ done: 0 });
    const t0 = performance.now();
    try {
      const out = await decodeVideo(file, { signal: controller.signal, onProgress: setProgress });
      setResult({ ...out, seconds: (performance.now() - t0) / 1000 });
      setState("done");
    } catch (e) {
      if (controller.signal.aborted) {
        setState("idle");
      } else {
        setError(errorMessage(e));
        setState("error");
      }
    } finally {
      abort.current = null;
    }
  };

  const decoding = state === "decoding";

  return (
    <P.Card title="Decode a video" headerVisual="download">
      <div className="fv-panel">
        <DropZone
          accept="video/*,.mp4,.mkv,.webm,.mov"
          file={file}
          onFile={choose}
          disabled={decoding}
          prompt="Choose a FrameVault video (1080p)"
        />

        {decoding && progress && (
          <Progress
            value={progress.total ? (progress.done / progress.total) * 100 : undefined}
            label={
              progress.total
                ? `Reading frame ${nf.format(progress.done)} of ${nf.format(progress.total)}` +
                  (progress.filename ? ` · ${progress.filename}` : "")
                : "Looking for the metadata frames…"
            }
          />
        )}

        {state === "error" && (
          <P.Banner variant="critical" title="We couldn't decode this video">
            {`${error} Make sure it's a 1080p FrameVault video.`}
          </P.Banner>
        )}

        {result && (
          <div className="fv-result">
            {result.shaOk ? (
              <P.Banner variant="success" title="Integrity check passed">
                {`We recovered ${result.filename} and its SHA-256 matches the original.`}
              </P.Banner>
            ) : (
              <P.Banner variant="critical" title="Integrity check failed">
                {`We recovered ${result.filename}, but its SHA-256 doesn't match the original. You can still download it to take a look.`}
              </P.Banner>
            )}
            <div className="fv-stats">
              <Stat label="File size" value={formatBytes(result.size)} />
              <Stat label="Frames read" value={nf.format(result.validFrames)} />
              <Stat label="Took" value={formatDuration(result.seconds)} />
            </div>
            <div className="fv-hash">
              <span className="text-body-small fg-muted">SHA-256</span>
              <code className="text-code-inline">{result.sha256}</code>
            </div>
          </div>
        )}

        <div className="fv-actions">
          {decoding ? (
            <P.Button leadingVisual="stop" onClick={() => abort.current?.abort()}>
              Stop
            </P.Button>
          ) : result && fileURL ? (
            <P.Button
              variant="primary"
              leadingVisual="download"
              onClick={() => download(fileURL, result.filename)}
            >
              Download file
            </P.Button>
          ) : (
            <P.Button
              variant="primary"
              leadingVisual="play"
              disabled={!file || !engineReady}
              onClick={start}
            >
              Decode video
            </P.Button>
          )}
        </div>
      </div>
    </P.Card>
  );
}

// ---- app ----

function EngineBanner({ engine, onRetry }) {
  if (engine.status === "loading") {
    return (
      <P.Banner variant="info" title="Getting ffmpeg ready">
        We're loading ffmpeg.wasm (about 32 MB, once). You can pick files in the meantime.
      </P.Banner>
    );
  }
  if (engine.status === "error") {
    return (
      <P.Banner
        variant="critical"
        title="We couldn't start ffmpeg"
        actions={
          <P.Button size="small" leadingVisual="sync" onClick={onRetry}>
            Try again
          </P.Button>
        }
      >
        {engine.error}
      </P.Banner>
    );
  }
  return null;
}

export function App() {
  const [theme, setTheme] = useState(document.documentElement.dataset.theme);
  const [codecInfo, setCodecInfo] = useState(null);
  const [engine, setEngine] = useState({ status: "loading" });

  const startEngine = useCallback(() => {
    setEngine({ status: "loading" });
    Promise.all([loadCodec(), loadFFmpeg()])
      .then(([info]) => {
        setCodecInfo(info);
        setEngine({ status: "ready" });
      })
      .catch((e) => setEngine({ status: "error", error: errorMessage(e) }));
  }, []);

  useEffect(() => {
    loadCodec().then(setCodecInfo, () => {});
    startEngine();
  }, [startEngine]);

  const pickTheme = (i) => {
    const id = THEMES[i].id;
    document.documentElement.dataset.theme = id;
    localStorage.setItem("framevault-theme", id);
    setTheme(id);
  };

  const ready = engine.status === "ready";

  return (
    <div className="fv-shell">
      <header className="fv-header">
        <div className="fv-title">
          <h1 className="text-title-large">FrameVault</h1>
          <p className="text-body-large fg-muted">
            Turn any file into a video that survives YouTube, then turn it back. It all runs
            in this tab: your files never leave your computer.
          </p>
        </div>
        <div className="fv-header-side">
          <P.SegmentedControl
            aria-label="Theme"
            size="small"
            items={THEMES.map(({ label, icon }) => (icon ? { label, icon } : label))}
            selectedIndex={Math.max(0, THEMES.findIndex((t) => t.id === theme))}
            onChange={pickTheme}
          />
          {ready && (
            <P.Chip tone="success" leadingVisual="check-circle">
              ffmpeg ready
            </P.Chip>
          )}
        </div>
      </header>

      <EngineBanner engine={engine} onRetry={startEngine} />

      <main className="fv-grid">
        <EncodePanel engineReady={ready} />
        <DecodePanel engineReady={ready} />
      </main>

      <footer className="fv-footer text-body-small fg-muted">
        <p>
          Each frame is a 30 × 16 grid of black and white blocks, bound together with
          Reed-Solomon error correction, so the data survives re-encoding. The codec is
          FrameVault&apos;s Rust core compiled to WebAssembly
          {codecInfo ? ` (v${codecInfo.version})` : ""}; ffmpeg.wasm does the video.
        </p>
        <p>
          <a href="https://github.com/Advik-B/FrameVault">Source on GitHub</a>
          {" · "}
          Prefer the command line? Grab a build from the{" "}
          <a href="https://github.com/Advik-B/FrameVault/releases">releases page</a>.
        </p>
      </footer>
    </div>
  );
}
