// Imports `dist/test/net_hints.mjs` — the unminified second output esbuild
// emits precisely so this file can reach the module (see esbuild.config.mjs).

import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { dirname, resolve } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const modulePath = resolve(here, "..", "dist", "test", "net_hints.mjs");
if (!existsSync(modulePath)) {
  throw new Error(
    `net hints test bundle missing at ${modulePath} — package.json's test script ` +
      `runs \`node esbuild.config.mjs\` before tests; rebuild and retry.`,
  );
}
const { annotateBrowserError } = await import(modulePath);

const DOCKER = {
  mode: "docker",
  workMountSource: "/data/aura/.baybo/work",
  profileMountSource: "/data/aura/.baybo/state/browser/profile",
  hostGatewayHint: "host.docker.internal",
};

const ok = (text) => ({ content: [{ type: "text", text }] });

const err = (text) => ({ isError: true, content: [{ type: "text", text }] });
const textOf = (r) =>
  r.content
    .filter((b) => b.type === "text")
    .map((b) => b.text)
    .join("\n");

test("host mode is never annotated — the namespaces are the agent's own", () => {
  const input = err("Error: net::ERR_CONNECTION_REFUSED at http://127.0.0.1:8877/");
  assert.equal(annotateBrowserError(input, { mode: "host" }), input);
});

test("connection refused on loopback explains the network namespace", () => {
  const out = annotateBrowserError(
    err("Error: net::ERR_CONNECTION_REFUSED at http://127.0.0.1:8877/x.html"),
    DOCKER,
  );
  const t = textOf(out);
  assert.match(t, /own network namespace/);
  assert.match(t, /almost certainly the namespace split/);
  assert.match(t, /host\.docker\.internal/);
  assert.match(t, /0\.0\.0\.0/, "binding is one of the two invisible preconditions");
  assert.match(t, /firewall/, "the other precondition");
});

test("a non-loopback host drops the loopback-specific claim", () => {
  const t = textOf(
    annotateBrowserError(err("Error: net::ERR_NAME_NOT_RESOLVED at http://intranet/x"), DOCKER),
  );
  assert.doesNotMatch(t, /almost certainly the namespace split/);
  assert.match(t, /own network namespace/);
});

test("file not found remaps a work-dir path to its container path", () => {
  const t = textOf(
    annotateBrowserError(
      err(
        "Error: net::ERR_FILE_NOT_FOUND at file:///data/aura/.baybo/work/report/a16z-Decagon.html",
      ),
      DOCKER,
    ),
  );
  assert.match(t, /file:\/\/\/data\/work\/report\/a16z-Decagon\.html/);
});

test("percent-escaped CJK filenames survive the remap", () => {
  const t = textOf(
    annotateBrowserError(
      err(
        "Error: net::ERR_FILE_NOT_FOUND at file:///data/aura/.baybo/work/a16z-Decagon%E4%B8%AD%E6%96%87.html",
      ),
      DOCKER,
    ),
  );
  assert.match(t, /file:\/\/\/data\/work\/a16z-Decagon中文\.html/);
});

test("a path outside every mount says so instead of inventing a remap", () => {
  const t = textOf(
    annotateBrowserError(err("Error: net::ERR_FILE_NOT_FOUND at file:///etc/hosts"), DOCKER),
  );
  assert.match(t, /not under a mount/);
  assert.match(t, /docker cp/);
  assert.doesNotMatch(t, /file:\/\/\/data\/work\/etc/);
});

test("with no work mount the hint says host files are invisible, full stop", () => {
  const t = textOf(
    annotateBrowserError(err("Error: net::ERR_FILE_NOT_FOUND at file:///srv/x.html"), {
      mode: "docker",
      profileMountSource: "/p",
    }),
  );
  assert.match(t, /own filesystem/);
  assert.doesNotMatch(t, /\/data\/work/);
});

test("an unrelated error is passed through untouched", () => {
  const input = err("Error: Element uid 3_7 no longer exists on the page.");
  assert.equal(annotateBrowserError(input, DOCKER), input);
});

test("a successful result is untouched even in docker mode", () => {
  const result = ok("## Pages\n0: https://example.com");
  assert.equal(annotateBrowserError(result, DOCKER), result);
});

// The failure this guards: `list_console_messages` on a page whose own
// fetch failed, or a snapshot of an error page, carries `net::ERR_*` in a
// perfectly successful result. Annotating it would lecture the agent about
// container namespaces over a message the *page* produced.
test("net::ERR_ text inside a SUCCESSFUL result is not annotated", () => {
  const result = ok(
    "## Console messages\nGET https://cdn.example.com/x.js net::ERR_CONNECTION_REFUSED",
  );
  assert.equal(
    annotateBrowserError(result, DOCKER),
    result,
    "only isError results describe what the browser itself could not reach",
  );
});

test("the font mount is listed only when one was actually mounted", () => {
  const withFonts = textOf(
    annotateBrowserError(err("Error: net::ERR_FILE_NOT_FOUND at file:///srv/x.html"), {
      ...DOCKER,
      fontMountSource: "/data/aura/.baybo/work/.fonts",
    }),
  );
  assert.match(withFonts, /\/data\/fonts/);

  const withoutFonts = textOf(
    annotateBrowserError(err("Error: net::ERR_FILE_NOT_FOUND at file:///srv/x.html"), DOCKER),
  );
  assert.doesNotMatch(
    withoutFonts,
    /\/data\/fonts/,
    "the image bakes in an empty /data/fonts; naming it as a mount sends the agent to a dead end",
  );
});

// /w/work vs /w/workspace: a bare startsWith would remap the second as if
// it lived under the first and hand back a confidently wrong URL.
test("a sibling directory sharing the mount's name prefix is not remapped", () => {
  const t = textOf(
    annotateBrowserError(err("Error: net::ERR_FILE_NOT_FOUND at file:///data/aura/.baybo/workspace/x.html"), DOCKER),
  );
  assert.match(t, /not under a mount/);
  assert.doesNotMatch(t, /file:\/\/\/data\/work/);
});

test("isError is preserved so watchdog liveness accounting is unaffected", () => {
  const out = annotateBrowserError(
    err("Error: net::ERR_CONNECTION_REFUSED at http://localhost:1/"),
    DOCKER,
  );
  assert.equal(out.isError, true);
});

test("cdp_url mode warns that the browser may be another machine entirely", () => {
  const t = textOf(
    annotateBrowserError(err("Error: net::ERR_FILE_NOT_FOUND at file:///srv/x.html"), {
      mode: "cdp_url",
    }),
  );
  assert.match(t, /operator-managed Chrome/);
});

test("a docker host with no gateway alias does not promise a reachable address", () => {
  const t = textOf(
    annotateBrowserError(err("Error: net::ERR_CONNECTION_REFUSED at http://localhost:9/"), {
      mode: "docker",
    }),
  );
  assert.match(t, /no configured alias/);
  assert.doesNotMatch(t, /host\.docker\.internal/);
});
