// Self-contained bundler for the browser tool sidecar.
//
// Why esbuild instead of bun build: bun's bundler hard-fails on the
// optional `electron` and `chromium-bidi/...` requires that
// `playwright-core` ships for non-chromium backends — we never
// execute those code paths but bun won't bundle past them. esbuild
// treats unresolvable externals as runtime-resolved when listed in
// `external`, leaving a `require('electron')` call in the bundle that
// only fires (and only fails) if the agent ever asks for the
// electron backend, which we don't.
//
// playwright-core also reads its own version from a relative
// `../../../package.json` at runtime, which esbuild can't inline
// (the path is computed). We work around this by setting
// `PW_VERSION_OVERRIDE` / `PW_CLI_DISPLAY_VERSION` env vars BEFORE
// any playwright module loads — see the `banner` below — so the
// version-fetching require() is short-circuited.
//
// Output: dist/bundle.mjs — single self-contained ESM file. The
// gateway's build.rs zstd-compresses this and embeds it as a
// `SidecarAsset`, so node_modules is no longer needed at runtime.

import { build } from "esbuild";
import { readFileSync, readdirSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));

// Read playwright-core's version at build time so the bundled
// `PW_VERSION_OVERRIDE` matches whatever version pnpm resolved.
function readPlaywrightVersion() {
  // pnpm hoists deps under node_modules/.pnpm/<name>@<ver>/node_modules/<name>/.
  // playwright-core is a transitive dep of playwright; both have a
  // package.json we can read from there.
  const candidates = [
    resolve(here, "node_modules/playwright-core/package.json"),
    resolve(here, "../../node_modules/playwright-core/package.json"),
  ];
  for (const p of candidates) {
    try {
      const pkg = JSON.parse(readFileSync(p, "utf8"));
      if (pkg.version) return pkg.version;
    } catch {}
  }
  // Fall back to the .pnpm directory layout.
  // Synchronously walk node_modules/.pnpm for the first
  // playwright-core@X.Y.Z entry.
  try {
    const pnpmDir = resolve(here, "../../node_modules/.pnpm");
    for (const entry of readdirSync(pnpmDir)) {
      if (entry.startsWith("playwright-core@")) {
        const inner = resolve(pnpmDir, entry, "node_modules/playwright-core/package.json");
        try {
          const pkg = JSON.parse(readFileSync(inner, "utf8"));
          if (pkg.version) return pkg.version;
        } catch {}
      }
    }
  } catch {}
  // Last-resort placeholder. UA strings don't matter for correctness.
  return "0.0.0-bundled";
}

const PW_VERSION = readPlaywrightVersion();
console.log(`bundling with playwright-core version ${PW_VERSION}`);

/**
 * playwright-core's `nodePlatform.js` has one line that esbuild
 * can't statically inline:
 *
 *   const coreDir = path.dirname(require.resolve("../../../package.json"));
 *
 * `coreDir` is used purely for stack-trace pretty-printing
 * (boxedStackPrefixes). Replacing the require.resolve with a
 * literal nonexistent-path string is harmless: stack traces look
 * identical except they don't auto-trim playwright frames, which
 * is something we'd actually prefer in error reports anyway.
 *
 * This plugin uses `onLoad` to rewrite the source of just that one
 * file before esbuild bundles it.
 */
const playwrightCoreDirStub = {
  name: "playwright-coreDir-stub",
  setup(build) {
    build.onLoad(
      { filter: /playwright-core\/lib\/server\/utils\/nodePlatform\.js$/ },
      async (args) => {
        const src = readFileSync(args.path, "utf8");
        const needle = 'require.resolve("../../../package.json")';
        if (!src.includes(needle)) {
          return null;
        }
        const replaced = src.replace(
          needle,
          '"/__playwright_core_bundled__/package.json"',
        );
        return { contents: replaced, loader: "js" };
      },
    );
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
  // playwright-core conditionally pulls in non-chromium backends
  // (firefox / webkit / electron / bidi). We only ever drive
  // chromium, so leaving these as runtime-resolved externals is
  // safe — the require() never fires in our code path. esbuild
  // happily emits the bundle with these unresolved.
  external: [
    "electron",
    "chromium-bidi",
    "chromium-bidi/*",
    // playwright-core's optional deps it tries to load for backend
    // discovery / OS-specific bits.
    "fsevents",
  ],
  // playwright-core has plenty of dynamic requires that look like
  // `require(name)` where name is computed; esbuild warns on these
  // by default. We accept the warnings — the dynamic paths are
  // backend selection that resolves at runtime against the actual
  // installed playwright tree (which ships alongside the bundle's
  // host env via the supervisor's spawn cwd, OR the bundle's own
  // imports if fully self-contained).
  // Prepended to the bundle output; runs before any bundled
  // module body.
  //
  // - `createRequire`: esbuild's ESM output uses dynamic
  //   `require()` for Node built-ins (events, crypto, …) and
  //   plain Node ESM doesn't expose `require`. This shim makes
  //   one available at module scope so the bundle runs under
  //   plain `node ./bundle.mjs` AND under `bun ./bundle.mjs`.
  // - `PW_VERSION_OVERRIDE` / `PW_CLI_DISPLAY_VERSION`: short-
  //   circuit the playwright-core relative-path package.json
  //   reads so we don't need the file at runtime.
  banner: {
    js: [
      `import { createRequire as __auraCreateRequire } from "node:module";`,
      `import { fileURLToPath as __auraFileURLToPath } from "node:url";`,
      `import { dirname as __auraDirname } from "node:path";`,
      `const require = __auraCreateRequire(import.meta.url);`,
      `const __filename = __auraFileURLToPath(import.meta.url);`,
      `const __dirname = __auraDirname(__filename);`,
      `if (typeof process !== "undefined" && process.env) {`,
      `  process.env.PW_VERSION_OVERRIDE = process.env.PW_VERSION_OVERRIDE || ${JSON.stringify(PW_VERSION)};`,
      `  process.env.PW_CLI_DISPLAY_VERSION = process.env.PW_CLI_DISPLAY_VERSION || ${JSON.stringify(PW_VERSION)};`,
      `}`,
    ].join("\n"),
  },
  plugins: [playwrightCoreDirStub],
  logLevel: "info",
  logOverride: {
    "unsupported-dynamic-import": "silent",
    "unsupported-require-call": "silent",
    // playwright-core uses `eval` in a couple of dynamic-context
    // helpers; we accept the warning since the eval is internal
    // and not driven by user input.
    "direct-eval": "silent",
  },
});
const ms = Date.now() - start;
const warnings = result.warnings.length;
const errors = result.errors.length;
console.log(`bundled in ${ms}ms (${warnings} warnings, ${errors} errors)`);
if (errors > 0) {
  process.exit(1);
}

// Test-only build: a separate ESM bundle that re-exports the
// `network_policy` module so node:test can import its functions
// without depending on Playwright (which would force chromium /
// real network). The test entry is a one-liner re-export at
// `test/_entry.mjs`; we just bundle it into `dist/`.
const testStart = Date.now();
const testResult = await build({
  entryPoints: ["test/_entry.mjs"],
  bundle: true,
  format: "esm",
  platform: "node",
  target: "node20",
  outfile: "dist/network_policy_test.mjs",
  minify: false,
  sourcemap: false,
  external: [],
  logLevel: "warning",
});
console.log(
  `test bundle in ${Date.now() - testStart}ms (${testResult.warnings.length} warnings, ${testResult.errors.length} errors)`,
);
if (testResult.errors.length > 0) {
  process.exit(1);
}
