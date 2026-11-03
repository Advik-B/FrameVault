// Vite plugin: compiles Purrimer's design-system bundle into real ES modules.
//
// Purrimer ships `bundle.js` as a classic script: two IIFEs (the core components, then the
// Qt widgets) that read React from `window.React` and publish everything on
// `window.Purrimer`. Loaded as-is, that's ~310 KB of unminified code, every component
// whether used or not, plus a global React. This plugin rewrites it at build time into:
//
//   purrimer/core  the first IIFE as a module: `import React from "react"`, named exports
//   purrimer/qt    the second IIFE: same, and `import * as P from "purrimer/core"`
//   purrimer       `export *` from both
//
// so the app imports components like any npm package (`import * as P from "purrimer"`),
// Rollup tree-shakes the ones it doesn't use, and the rest is minified with the app.
// `bundle.js` itself stays byte-for-byte what the design system exports, so updating
// Purrimer is still a drop-in copy. If its format ever changes, the build fails here with
// a clear message instead of shipping something broken.

import { readFileSync } from "node:fs";

const PUBLIC_IDS = { purrimer: "\0purrimer", "purrimer/core": "\0purrimer/core", "purrimer/qt": "\0purrimer/qt" };

function fail(msg) {
  throw new Error(`[purrimer] ${msg}. Has bundle.js's format changed?`);
}

function replaceOnce(src, pattern, replacement, what) {
  const matches = src.match(new RegExp(pattern.source, pattern.flags.replace("g", "") + "g"));
  if (!matches || matches.length !== 1) fail(`expected exactly one ${what}, found ${matches?.length ?? 0}`);
  return src.replace(pattern, replacement);
}

/** One IIFE body -> an ES module. */
function toModule(body, { imports }) {
  let out = body;
  out = replaceOnce(out, /^ {2}"use strict";\n/m, "", '"use strict" directive');
  out = replaceOnce(out, /^ {2}var React = window\.React;\n/m, "", "window.React read");
  if (imports.includes("purrimer/core")) {
    out = replaceOnce(out, /^ {2}var P = window\.Purrimer;\n/m, "", "window.Purrimer read");
  }

  // `window.Purrimer = Object.assign(window.Purrimer || {}, { Name: expr, ... });`
  const exportBlock = /^ {2}window\.Purrimer = Object\.assign\(window\.Purrimer \|\| \{\}, \{\n([\s\S]*?)\n {2}\}\);$/m;
  const m = out.match(exportBlock);
  if (!m) fail("couldn't find the window.Purrimer export block");
  const named = [];
  const computed = [];
  for (const line of m[1].split("\n")) {
    const entry = line.match(/^\s*([A-Za-z_$][\w$]*):\s*(.+?),?\s*$/);
    if (!entry) fail(`unexpected export entry: ${line.trim()}`);
    const [, key, expr] = entry;
    if (expr === key) named.push(key);
    else computed.push(`export const ${key} = ${expr};`);
  }
  out = replaceOnce(out, exportBlock, [`export { ${named.join(", ")} };`, ...computed].join("\n"), "export block");

  return `${imports}\n${out}\n`;
}

function compile(file) {
  const src = readFileSync(file, "utf8");
  const parts = [...src.matchAll(/^\(function \(\) \{\n([\s\S]*?)^\}\)\(\);$/gm)].map((m) => m[1]);
  if (parts.length !== 2) fail(`expected 2 top-level IIFEs (core + Qt widgets), found ${parts.length}`);
  return {
    "\0purrimer/core": toModule(parts[0], { imports: 'import React from "react";' }),
    "\0purrimer/qt": toModule(parts[1], {
      imports: 'import React from "react";\nimport * as P from "purrimer/core";',
    }),
    "\0purrimer": 'export * from "purrimer/core";\nexport * from "purrimer/qt";\n',
  };
}

/** @param {{ bundle: string }} options  absolute path to Purrimer's bundle.js */
export default function purrimer({ bundle }) {
  let modules;
  return {
    name: "purrimer",
    enforce: "pre",
    buildStart() {
      modules = compile(bundle);
      this.addWatchFile(bundle);
    },
    watchChange(id) {
      if (id.replaceAll("\\", "/") === bundle.replaceAll("\\", "/")) modules = compile(bundle);
    },
    resolveId(id) {
      // Pure component library: whatever isn't imported can be dropped entirely.
      if (id in PUBLIC_IDS) return { id: PUBLIC_IDS[id], moduleSideEffects: false };
    },
    load(id) {
      if (id in (modules ?? {})) return modules[id];
    },
  };
}
