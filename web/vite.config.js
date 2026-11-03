import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { fileURLToPath } from "node:url";

import purrimer from "./plugins/purrimer.js";

// ffmpeg.wasm's multi-threaded core needs SharedArrayBuffer, which browsers only expose
// to cross-origin-isolated pages. The dev and preview servers send the headers directly;
// static hosts that can't (GitHub Pages) get them from public/coi-serviceworker.js.
const isolation = {
  "Cross-Origin-Opener-Policy": "same-origin",
  "Cross-Origin-Embedder-Policy": "require-corp",
};

export default defineConfig({
  // Relative asset URLs, so the build works under any path (e.g. /FrameVault/ on Pages).
  base: "./",
  // `purrimer` compiles the design system's bundle.js into tree-shakeable ES modules.
  plugins: [
    purrimer({ bundle: fileURLToPath(new URL("./src/purrimer/bundle.js", import.meta.url)) }),
    react(),
  ],
  server: { headers: isolation },
  preview: { headers: isolation },
  // @ffmpeg/ffmpeg spawns its worker via `new URL(..., import.meta.url)`; pre-bundling
  // breaks that path, so serve these packages as-is.
  optimizeDeps: { exclude: ["@ffmpeg/ffmpeg", "@ffmpeg/util"] },
  worker: { format: "es" },
  build: { target: "es2022" },
});
