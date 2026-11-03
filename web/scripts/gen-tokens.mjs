// Generates src/purrimer/tokens.css from Purrimer's tokens.json — the CSS custom
// properties, @font-face rules and text-style classes that bundle.css reads.
//
//   :root                 every token; themed ones take the first used theme's value
//   [data-theme="<id>"]   overrides for each further used theme (missing values inherit :root)
//
// The demo uses only Purrimer's greyscale pair, Blanc (default) and Noir; the other themes
// in tokens.json aren't emitted.
//   .text-<style>         one class per type style, in its family

import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const dir = join(dirname(fileURLToPath(import.meta.url)), "..", "src", "purrimer");
const tokens = JSON.parse(readFileSync(join(dir, "tokens.json"), "utf8"));

const USED_THEMES = ["blanc", "noir"];
// Per tokens.json's rules, a token with no value for a theme inherits the file's FIRST theme.
const fileDefault = tokens.color.themes[0].id;
const themes = USED_THEMES.map((id) => tokens.color.themes.find((t) => t.id === id));
const [primary, ...others] = themes;
const themed = (value, id) => value[id] ?? value[fileDefault] ?? Object.values(value)[0];
const isDark = (t) => /dark|night/i.test(t.name);

/** `{other-token}` aliases become var() references; everything else passes through. */
const cssValue = (v) =>
  typeof v === "string" ? v.replace(/\{([A-Za-z0-9][A-Za-z0-9_.-]*)\}/g, "var(--$1)") : String(v);

const kebab = (s) => s.replace(/[A-Z]/g, (c) => "-" + c.toLowerCase());

// Every family shaped `{ tokens: [{ name, value }] }`; `type` is handled separately.
const families = Object.entries(tokens).filter(
  ([key, fam]) => key !== "type" && fam && Array.isArray(fam.tokens),
);

const root = [];
const perTheme = Object.fromEntries(others.map((t) => [t.id, []]));

for (const [, fam] of families) {
  for (const { name, value } of fam.tokens) {
    if (value && typeof value === "object") {
      root.push(`  --${name}: ${cssValue(themed(value, primary.id))};`);
      for (const t of others) {
        perTheme[t.id].push(`  --${name}: ${cssValue(themed(value, t.id))};`);
      }
    } else {
      root.push(`  --${name}: ${cssValue(value)};`);
    }
  }
}

for (const [key, stack] of Object.entries(tokens.type.families)) {
  root.push(`  --font-${key}: ${stack};`);
}

const fontFaces = tokens.type.fonts.map(
  (f) => `@font-face {
  font-family: "${f.family}";
  src: url("./${f.file}") format("woff2");
  font-weight: ${f.weight};
  font-style: ${f.style ?? "normal"};
  font-display: swap;
}`,
);

const textClasses = tokens.type.groups.flatMap((group) =>
  group.styles.map((style) => {
    const decls = [`  font-family: var(--font-${group.family});`];
    for (const [k, v] of Object.entries(style)) {
      if (["name", "sample", "usage"].includes(k)) continue;
      decls.push(`  ${kebab(k)}: ${v};`);
    }
    if (group.family === "numeric") {
      decls.push("  font-variant-numeric: tabular-nums slashed-zero;");
    }
    return `.${style.name} {\n${decls.join("\n")}\n}`;
  }),
);

const css = `/* ${tokens.name} — generated from tokens.json by scripts/gen-tokens.mjs. Do not edit. */

${fontFaces.join("\n\n")}

:root {
  color-scheme: ${isDark(primary) ? "dark" : "light"};
${root.join("\n")}
}

${others
  .map(
    (t) => `[data-theme="${t.id}"] {
  color-scheme: ${isDark(t) ? "dark" : "light"};
${perTheme[t.id].join("\n")}
}`,
  )
  .join("\n\n")}

${textClasses.join("\n\n")}
`;

writeFileSync(join(dir, "tokens.css"), css);
console.log(
  `tokens.css: ${root.length} tokens, ${others.length} theme overrides, ${textClasses.length} text styles`,
);
