// Copies ffmpeg.wasm's multi-threaded core into public/ffmpeg/ so it's served byte-for-byte.
// Importing it through Vite (`?url`) isn't enough in dev: Vite rewrites the core's classic
// pthread worker script (injecting an ES `import`), which breaks every ffmpeg thread.

import { copyFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

// The package exports its ESM entry (dist/esm/ffmpeg-core.js); the other two sit beside it.
const src = dirname(fileURLToPath(import.meta.resolve("@ffmpeg/core-mt")));
const dest = join(dirname(fileURLToPath(import.meta.url)), "..", "public", "ffmpeg");

mkdirSync(dest, { recursive: true });
for (const f of ["ffmpeg-core.js", "ffmpeg-core.wasm", "ffmpeg-core.worker.js"]) {
  copyFileSync(join(src, f), join(dest, f));
}
console.log(`ffmpeg core copied to ${dest}`);
