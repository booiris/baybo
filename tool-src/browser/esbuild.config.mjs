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
// alongside the bundle (declared via `baybo.auxAssets` in
// package.json). At runtime the layout in
// `$XDG_CACHE_HOME/baybo/sidecars/browser-<hash>/` is:
//
//   bundle.mjs            <- this output (esbuilds src/server.ts)
//   cddm/build/src/...    <- CDDM's published artifact, copied verbatim
//
// `src/server.ts` imports `chrome-devtools-mcp` by its package name;
// esbuild rewrites that to a relative `./cddm/build/src/index.js`
// path via the `bayboResolveCddm` plugin below.

import { build } from "esbuild";
import { cpSync, existsSync, mkdirSync, readFileSync, rmSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const cddmSrc = resolve(here, "node_modules/chrome-devtools-mcp/build");
const distDir = resolve(here, "dist");
const cddmDist = resolve(distDir, "cddm/build");

if (!existsSync(cddmSrc)) {
  console.error(
    "chrome-devtools-mcp/build/ missing — run `pnpm install --filter @baybo/tool-browser` first",
  );
  process.exit(1);
}

// Inline Mozilla's Readability.js source so `read_page.ts` can inject
// it into Chrome via `evaluate_script` without a network round-trip.
// See `src/read_page.ts` for the injection wrapper.
const readabilityPath = resolve(
  here,
  "node_modules/@mozilla/readability/Readability.js",
);
if (!existsSync(readabilityPath)) {
  console.error(
    "@mozilla/readability/Readability.js missing — run `pnpm install --filter @baybo/tool-browser` first",
  );
  process.exit(1);
}
const readabilitySource = readFileSync(readabilityPath, "utf8");

mkdirSync(distDir, { recursive: true });
rmSync(resolve(distDir, "cddm"), { recursive: true, force: true });
mkdirSync(cddmDist, { recursive: true });
cpSync(cddmSrc, cddmDist, { recursive: true });

const bayboResolveCddm = {
  name: "baybo-resolve-cddm",
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
  define: {
    // Build-time constant referenced by `src/read_page.ts`. Wrapped in
    // a string-literal so the source becomes a JS expression at bundle
    // time, then ends up as a string runtime value.
    __READABILITY_SOURCE__: JSON.stringify(readabilitySource),
  },
  banner: {
    js: [
      `import { createRequire as __bayboCreateRequire } from "node:module";`,
      `import { fileURLToPath as __bayboFileURLToPath } from "node:url";`,
      `import { dirname as __bayboDirname } from "node:path";`,
      `const require = __bayboCreateRequire(import.meta.url);`,
      `const __filename = __bayboFileURLToPath(import.meta.url);`,
      `const __dirname = __bayboDirname(__filename);`,
    ].join("\n"),
  },
  plugins: [bayboResolveCddm],
  logLevel: "info",
});
const ms = Date.now() - start;
console.log(
  `bundled in ${ms}ms (${result.warnings.length} warnings, ${result.errors.length} errors)`,
);
if (result.errors.length > 0) {
  process.exit(1);
}
