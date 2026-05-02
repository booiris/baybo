// Bundler for the browser tool sidecar.
//
// We delegate the actual MCP surface to chrome-devtools-mcp. That
// package ships pre-bundled (74 JS files in its `build/` dir, no
// runtime deps in its package.json — everything is rolled in by
// rollup at publish time). Re-bundling those 19 MB of pre-rolled
// output through esbuild is fragile: lighthouse pulls in
// `chrome-devtools-frontend` with worker scripts, and tiktoken loads
// a WASM blob via fs paths. Both patterns survive a lazy "ship the
// whole `build/` tree as aux assets" but break under "smash
// everything into one .mjs".
//
// So our `dist/bundle.mjs` is intentionally tiny: it imports from a
// sibling `cddm/build/src/index.js` that the gateway materialises
// alongside the bundle (declared via `aura.auxAssets` in
// package.json). At runtime the layout in
// `$XDG_CACHE_HOME/aura/sidecars/browser-<hash>/` is:
//
//   bundle.mjs            <- this output (esbuilds src/server.ts)
//   cddm/build/src/...    <- CDDM's published artifact, copied verbatim
//
// `src/server.ts` imports `chrome-devtools-mcp` by its package name;
// esbuild rewrites that to a relative `./cddm/build/src/index.js`
// path via the `auraResolveCddm` plugin below.

import { build } from "esbuild";
import { cpSync, existsSync, mkdirSync, rmSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const cddmSrc = resolve(here, "node_modules/chrome-devtools-mcp/build");
const distDir = resolve(here, "dist");
const cddmDist = resolve(distDir, "cddm/build");

if (!existsSync(cddmSrc)) {
  console.error(
    "chrome-devtools-mcp/build/ missing — run `pnpm install --filter @aura/tool-browser` first",
  );
  process.exit(1);
}

mkdirSync(distDir, { recursive: true });
rmSync(resolve(distDir, "cddm"), { recursive: true, force: true });
mkdirSync(cddmDist, { recursive: true });
cpSync(cddmSrc, cddmDist, { recursive: true });

const auraResolveCddm = {
  name: "aura-resolve-cddm",
  setup(build) {
    build.onResolve({ filter: /^chrome-devtools-mcp$/ }, () => ({
      path: "./cddm/build/src/index.js",
      external: true,
    }));
  },
};

const start = Date.now();
const result = await build({
  entryPoints: ["src/server.ts"],
  bundle: true,
  format: "esm",
  platform: "node",
  target: "node20",
  outfile: "dist/bundle.mjs",
  minify: true,
  sourcemap: false,
  // The MCP SDK is a small pure-ESM lib — let esbuild inline it. CDDM
  // is materialised next to the bundle as a separate dir tree, hence
  // the resolver plugin above.
  external: [],
  banner: {
    js: [
      `import { createRequire as __auraCreateRequire } from "node:module";`,
      `import { fileURLToPath as __auraFileURLToPath } from "node:url";`,
      `import { dirname as __auraDirname } from "node:path";`,
      `const require = __auraCreateRequire(import.meta.url);`,
      `const __filename = __auraFileURLToPath(import.meta.url);`,
      `const __dirname = __auraDirname(__filename);`,
    ].join("\n"),
  },
  plugins: [auraResolveCddm],
  logLevel: "info",
});
const ms = Date.now() - start;
console.log(
  `bundled in ${ms}ms (${result.warnings.length} warnings, ${result.errors.length} errors)`,
);
if (result.errors.length > 0) {
  process.exit(1);
}
