import { createHash } from 'node:crypto';
import { readFileSync, readdirSync, statSync, writeFileSync } from 'node:fs';
import { join, posix, relative, resolve, sep } from 'node:path';
import type { Plugin, ResolvedConfig } from 'vite';

/**
 * Emits `dist/sw.js` from `pwa/service-worker.js` with the precache list and a
 * content-derived build id substituted in.
 *
 * Why by hand rather than `vite-plugin-pwa`: that plugin pulls workbox, and
 * with it 249 transitive packages into a workspace whose whole point is a
 * dependency-light dashboard — to generate a worker this app needs about 130
 * lines of. Doing it here also means the precache list is chosen against the
 * gateway's real asset semantics (see `service-worker.js`) instead of a glob.
 *
 * It runs in `closeBundle`, the one hook that fires after Vite has copied
 * `public/` into the output, so the list is built from the actual `dist/` tree
 * rather than from the rollup bundle (which never sees `public/`).
 */

const SW_SOURCE = 'pwa/service-worker.js';
const SW_OUTPUT = 'sw.js';

/**
 * Precached extensions. Everything the shell needs to paint and run offline,
 * and nothing else:
 *   - `.woff2` only — KaTeX's stylesheet also ships `.woff`/`.ttf` fallbacks
 *     (~876 KB of the 1.03 MB font payload) that no browser able to run this
 *     React 19 bundle will ever request. They stay network-only.
 *   - `.png`/`.ico` are launcher/tab icons the OS fetches, not the app; the
 *     runtime cache picks them up if the page ever asks.
 */
const PRECACHE_EXTENSIONS = ['.html', '.js', '.css', '.woff2', '.svg', '.webmanifest'];

/** The shell the worker falls back to. Its absence means the build is broken. */
const REQUIRED_ENTRY = '/index.html';

const BUILD_ID_PLACEHOLDER = "'__BUILD_ID__'";
const PRECACHE_PLACEHOLDER = "'__PRECACHE__'";

export function serviceWorkerPlugin(): Plugin {
  let config: ResolvedConfig;

  return {
    name: 'baybo:service-worker',
    apply: 'build',
    configResolved(resolved) {
      config = resolved;
    },
    closeBundle() {
      const outDir = resolve(config.root, config.build.outDir);
      const files = listFiles(outDir).filter((rel) => rel !== SW_OUTPUT);
      const precache = files
        .filter((rel) => PRECACHE_EXTENSIONS.some((ext) => rel.endsWith(ext)))
        .map((rel) => `/${rel}`)
        .sort();

      assertShellIsComplete(precache);
      assertThemeColorsAgree(outDir);

      const source = readFileSync(resolve(config.root, SW_SOURCE), 'utf8');
      const worker = substitute(
        substitute(source, BUILD_ID_PLACEHOLDER, JSON.stringify(buildId(outDir, precache))),
        PRECACHE_PLACEHOLDER,
        JSON.stringify(precache),
      );
      writeFileSync(join(outDir, SW_OUTPUT), worker);
      config.logger.info(
        `baybo:service-worker  ${SW_OUTPUT} precaches ${precache.length} files`,
      );
    },
  };
}

/**
 * Replaces a placeholder that must exist. Renaming a constant in the worker
 * without renaming it here would otherwise leave a literal `'__PRECACHE__'`
 * string in the shipped file, and `PRECACHE_URLS.map` would throw on install —
 * where nothing in the app is watching.
 */
function substitute(source: string, placeholder: string, value: string): string {
  if (!source.includes(placeholder)) {
    throw new Error(`baybo:service-worker: ${SW_SOURCE} has no ${placeholder} to substitute`);
  }
  return source.replace(placeholder, value);
}

/**
 * The precache list is the offline contract, and a silently-empty one looks
 * exactly like a working build until someone opens the app on a train. The
 * shell plus at least one script and one stylesheet is the floor that proves
 * the scan found the real output tree.
 */
function assertShellIsComplete(precache: string[]): void {
  const missing: string[] = [];
  if (!precache.includes(REQUIRED_ENTRY)) missing.push(REQUIRED_ENTRY);
  if (!precache.some((url) => url.endsWith('.js'))) missing.push('a script');
  if (!precache.some((url) => url.endsWith('.css'))) missing.push('a stylesheet');
  if (missing.length > 0) {
    throw new Error(
      `baybo:service-worker: precache list is missing ${missing.join(', ')} — ` +
        `refusing to ship a worker that would cache an empty shell`,
    );
  }
}

/**
 * The theme colour is written twice — `<meta name="theme-color">` for the
 * browser tab, `theme_color` in the manifest for the installed window — and
 * neither can reference the other (nor the `--color-brand` token they both
 * copy). Changing one and not the other gives an installed app whose title bar
 * disagrees with its own tab, which nobody notices until a screenshot.
 */
function assertThemeColorsAgree(outDir: string): void {
  const html = readFileSync(join(outDir, 'index.html'), 'utf8');
  const meta = /<meta\s+name="theme-color"\s+content="([^"]+)"/i.exec(html)?.[1];
  const manifest = JSON.parse(readFileSync(join(outDir, 'manifest.webmanifest'), 'utf8')) as {
    theme_color?: string;
  };
  if (meta === undefined) {
    throw new Error('baybo:service-worker: index.html has no theme-color meta tag');
  }
  if (meta.toLowerCase() !== manifest.theme_color?.toLowerCase()) {
    throw new Error(
      `baybo:service-worker: theme colour disagrees — index.html says ${meta}, ` +
        `manifest.webmanifest says ${manifest.theme_color ?? '(unset)'}`,
    );
  }
}

/**
 * Hash of every precached file's path AND bytes. Content-derived, never a
 * timestamp: the id names the cache, so a rebuild that changed nothing must
 * produce the same worker byte-for-byte — otherwise every `cargo build` would
 * hand users a "new version" prompt for an identical bundle.
 */
function buildId(outDir: string, precache: string[]): string {
  const hash = createHash('sha256');
  for (const url of precache) {
    hash.update(url);
    hash.update(readFileSync(join(outDir, url.slice(1))));
  }
  return hash.digest('hex').slice(0, 16);
}

/** Output-relative POSIX paths for every file under `dir`, recursively. */
function listFiles(dir: string): string[] {
  const out: string[] = [];
  const walk = (current: string): void => {
    for (const entry of readdirSync(current)) {
      const absolute = join(current, entry);
      if (statSync(absolute).isDirectory()) {
        walk(absolute);
      } else {
        out.push(relative(dir, absolute).split(sep).join(posix.sep));
      }
    }
  };
  walk(dir);
  return out;
}
