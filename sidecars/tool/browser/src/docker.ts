// Docker-spawn helpers for the browser sidecar.
//
// Lifecycle:
//   1. checkDockerAvailable() — `docker info` with a 3s timeout. Returns
//      `{ ok:false }` (with a reason string) when the daemon is missing,
//      down, or unreachable. Caller falls back to host-headless.
//   2. sweepStaleContainers() — list `baybo.role=browser-sidecar` labelled
//      containers, drop any whose `baybo.pid` label points at a dead
//      process. Cheap belt-and-braces against gateway crashes mid-run
//      (the `--rm` flag handles the graceful path).
//   3. spawnContainer() — compute the deterministic image tag (or take
//      the operator override), build the image if not present locally,
//      docker-run with labels + UID propagation + ephemeral CDP port +
//      optional VNC port, read back the published port via `docker port`,
//      poll `/json/version` until Chrome's CDP is live, return a handle.
//
// Why `--shm-size=2g`: the container's /dev/shm defaults to 64MB which
// crashes Chrome on non-trivial pages (real bug, not a precaution —
// see Puppeteer's troubleshooting docs). 2GB is the standard fix.
//
// Why `--user $(id -u):$(id -g)`: profile files written under
// /data/profile end up owned by the host operator's UID, so the same
// `$XDG_CACHE_HOME/baybo/browser/profile` round-trips cleanly between
// host-headless and docker modes. Without this, switching modes leaves
// the operator unable to read their own profile.
//
// Why `-p 127.0.0.1::9222`: ephemeral host-side port chosen by docker
// (read back via `docker port`), bound to loopback only. Multiple Baybo
// instances on one host don't collide; CDP isn't reachable from off-box.

import { execFile, spawn } from "node:child_process";
import { promisify } from "node:util";
import { createHash, randomBytes } from "node:crypto";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";

import { createLogger } from "./log.js";

const exec = promisify(execFile);

export type DockerPhase =
    | "docker-checking"
    | "docker-building-image"
    | "docker-starting-container"
    | "docker-waiting-for-cdp"
    | "ready";

export interface DockerSpawnOptions {
    dockerDir: string;
    imageTag?: string;
    profileDir: string;
    fontDir?: string;
    /**
     * Host directory bind-mounted read-only at {@link CONTAINER_WORK_DIR} so
     * the agent can open artefacts it just wrote via `file://`. Undefined
     * leaves the container with no view of the workspace at all.
     */
    workDir?: string;
    webVncPort?: number;
    /** `docker run --memory` value, e.g. `4g`. Undefined leaves the container uncapped. */
    memoryLimit?: string;
    /** Publish {@link HOST_GATEWAY_ALIAS} via `--add-host`. Requires Docker >= 20.10. */
    hostGatewayAlias?: boolean;
    /**
     * Chrome's `--disk-cache-size` in bytes, forwarded to the entrypoint so
     * both modes cap the (bind-mounted, never-swept) profile identically.
     */
    diskCacheBytes: number;
    viewport: { width: number; height: number };
    onPhase: (p: DockerPhase) => void;
    /**
     * Refuse to build the image, failing fast instead. Set on the watchdog's
     * repair path — see the `neverBuild` branch in `resolveImageTag`.
     */
    neverBuild?: boolean;
}

/** Where {@link DockerSpawnOptions.profileDir} lands inside the container. */
export const CONTAINER_PROFILE_DIR = "/data/profile";
/** Where {@link DockerSpawnOptions.fontDir} lands inside the container. */
export const CONTAINER_FONTS_DIR = "/data/fonts";
/** Where {@link DockerSpawnOptions.workDir} lands inside the container. */
export const CONTAINER_WORK_DIR = "/data/work";

/**
 * Cap on the container's **task** count. `--pids-limit` writes cgroup
 * `pids.max`, which counts threads, not processes — and Chrome is
 * lavishly threaded: a measured 7-tab browser sat at 386 tasks across 29
 * processes, roughly 55 tasks per tab. A cap sized as if it counted
 * processes would strangle a normal working set. 8192 leaves ~20x
 * headroom over that measurement while still stopping a fork bomb in
 * page JS before it reaches the host's pid space.
 */
const CONTAINER_PIDS_LIMIT = 8192;

export interface DockerHandle {
    cdpUrl: string;
    containerName: string;
    imageTag: string;
    stop: () => Promise<void>;
}

const logger = createLogger("[browser-mcp:docker]");
const log = logger.info;
const logDebug = logger.debug;

// How many trailing docker-build stderr lines to retain and surface when
// a build fails. Enough to carry the actual apt/network error without
// re-dumping the whole (multi-thousand-line) transcript.
const BUILD_STDERR_TAIL_LINES = 30;

/**
 * Ceiling on `docker build`. The log line above advertises 1-3 minutes; this is
 * generous against that while still being finite, which is the whole point —
 * an unbounded build on the watchdog's repair path takes the watchdog with it.
 */
const BUILD_TIMEOUT_MS = 15 * 60_000;

/**
 * The `host-gateway` magic value for `--add-host` landed in Docker 20.10.
 * Passing it to an older daemon fails the whole `docker run`, which would
 * demote the operator to host-headless over a diagnostic nicety — so the
 * flag is gated on the version we already read in
 * {@link checkDockerAvailable}. Unparseable versions are treated as too
 * old: losing the alias costs a hint, losing the container costs the mode.
 */
export function supportsHostGateway(serverVersion: string): boolean {
    const m = serverVersion.trim().match(/^(\d+)\.(\d+)/);
    if (!m || m[1] === undefined || m[2] === undefined) return false;
    const major = Number(m[1]);
    const minor = Number(m[2]);
    if (!Number.isFinite(major) || !Number.isFinite(minor)) return false;
    return major > 20 || (major === 20 && minor >= 10);
}

/** Docker's conventional alias for the host, published via `--add-host`. */
export const HOST_GATEWAY_ALIAS = "host.docker.internal";

export async function checkDockerAvailable(): Promise<
    { ok: true; serverVersion: string } | { ok: false; reason: string }
> {
    try {
        const { stdout } = await exec(
            "docker",
            ["info", "--format", "{{.ServerVersion}}"],
            { timeout: 3000 },
        );
        return { ok: true, serverVersion: stdout.trim() };
    } catch (e) {
        const reason =
            e instanceof Error ? e.message.split("\n")[0] ?? e.message : String(e);
        return { ok: false, reason };
    }
}

export async function sweepStaleContainers(): Promise<void> {
    let stdout: string;
    try {
        const r = await exec(
            "docker",
            [
                "ps",
                "-a",
                "--filter",
                "label=baybo.role=browser-sidecar",
                "--format",
                "{{.Names}}\t{{.Label \"baybo.pid\"}}",
            ],
            { timeout: 5000 },
        );
        stdout = r.stdout;
    } catch (e) {
        log(`sweep: docker ps failed (${(e as Error).message}); skipping`);
        return;
    }
    const lines = stdout
        .split("\n")
        .map((l) => l.trim())
        .filter((l) => l.length > 0);
    for (const line of lines) {
        const [name, pidStr] = line.split("\t");
        if (name === undefined || name.length === 0) continue;
        // `Number(pidStr)` returns NaN on partial parse; `parseInt("12abc",10)`
        // would silently truncate to 12 and falsely treat the container's
        // owner as still-alive when the label is corrupted.
        const pid = pidStr !== undefined && pidStr.length > 0 ? Number(pidStr) : NaN;
        if (Number.isFinite(pid) && pid > 0 && isPidAlive(pid)) {
            continue;
        }
        log(`sweep: removing stale container ${name} (owner pid=${pidStr !== undefined && pidStr.length > 0 ? pidStr : "?"} not alive)`);
        try {
            await exec("docker", ["rm", "-f", name], { timeout: 5000 });
        } catch (e) {
            log(`sweep: docker rm -f ${name} failed: ${(e as Error).message}`);
        }
    }
}

/**
 * Drop `baybo-browser:*` images that nothing references and that are past
 * {@link STALE_IMAGE_MAX_AGE_DAYS}.
 *
 * The image tag is a content hash of `Dockerfile` + `entrypoint.sh`, so
 * every edit to either mints a fresh ~1.3 GB image and the old one is
 * never looked at again. Nothing upstream reclaims them; a handful of
 * Dockerfile revisions is several gigabytes of dead layers.
 *
 * `keepTag` is the image this process is about to run — it may be newly
 * built and is therefore young, but pinning it explicitly means a clock
 * skew can't make us delete the image we depend on. `docker rmi` refuses
 * to remove an image a container still references, including exited
 * ones, so that case needs no special handling here.
 */
export async function sweepStaleImages(keepTag?: string): Promise<void> {
    let stdout: string;
    try {
        const r = await exec(
            "docker",
            ["images", "baybo-browser", "--format", "{{.Repository}}:{{.Tag}}\t{{.CreatedAt}}"],
            { timeout: 5000 },
        );
        stdout = r.stdout;
    } catch (e) {
        log(`image sweep: docker images failed (${(e as Error).message}); skipping`);
        return;
    }
    for (const line of stdout.split("\n").map((l) => l.trim()).filter((l) => l.length > 0)) {
        const [tag, createdAt] = line.split("\t");
        if (tag === undefined || tag.startsWith("baybo-browser:<none>")) continue;
        if (tag === keepTag) continue;
        if (createdAt === undefined || createdAt.length === 0) continue;
        const ageDays = parseDockerCreatedAtAgeDays(createdAt);
        if (ageDays === undefined || ageDays <= STALE_IMAGE_MAX_AGE_DAYS) continue;
        try {
            await exec("docker", ["rmi", tag], { timeout: 30_000 });
            log(`image sweep: removed ${tag} (${ageDays.toFixed(1)} days old, unreferenced)`);
        } catch (e) {
            // Almost always "image is referenced in multiple repositories"
            // or a container still holding it. Both are correct refusals.
            logDebug(`image sweep: docker rmi ${tag} skipped (${(e as Error).message.split("\n")[0]})`);
        }
    }
}

export function isPidAlive(pid: number): boolean {
    try {
        process.kill(pid, 0);
        return true;
    } catch (e) {
        // EPERM means the process EXISTS but belongs to another uid — we
        // are not allowed to signal it. Reading that as "dead" inverts the
        // check: the sweep would force-remove the container of a *running*
        // sidecar owned by a different user on the same host.
        return (e as NodeJS.ErrnoException).code === "EPERM";
    }
}

async function computeImageTag(dockerDir: string): Promise<string> {
    // Hashes Dockerfile + entrypoint only. Chrome's version isn't pinned
    // — `apt install google-chrome-stable_current_amd64.deb` picks up
    // whatever's stable at build time, and operators force a refresh by
    // `docker rmi`-ing the tag. Including a Chrome version here would
    // require a host-side resolver that doesn't exist for consumer
    // Chrome (Google publishes no equivalent of @puppeteer/browsers'
    // Chrome-for-Testing endpoint).
    const hash = createHash("sha256");
    hash.update(await readFile(join(dockerDir, "Dockerfile")));
    hash.update(await readFile(join(dockerDir, "entrypoint.sh")));
    return `baybo-browser:${hash.digest("hex").slice(0, 12)}`;
}

async function imageExists(tag: string): Promise<boolean> {
    try {
        await exec("docker", ["image", "inspect", tag], { timeout: 5000 });
        return true;
    } catch {
        return false;
    }
}

async function buildImage(dockerDir: string, tag: string): Promise<void> {
    log(`building image ${tag} (consumer google-chrome-stable); this takes 1-3 minutes on first boot`);
    const args = ["build", "-t", tag, dockerDir];
    // Capture (not inherit) the build's stderr. Inheriting piped the
    // whole apt/layer transcript — hundreds-to-thousands of lines — into
    // the gateway's own stderr and thence baybo.log at INFO. We keep only
    // a trailing window and surface it if the build fails, so a broken
    // build stays diagnosable without the success-path flood.
    // Every other docker call in this file goes through `exec(..., {timeout})`;
    // this one is the longest-running of the set and was the only one unbounded.
    // `spawn` ignores child_process's `timeout`, so the deadline has to ride an
    // AbortSignal. A CLI blocked on a wedged daemon socket, or a BuildKit step
    // waiting on a stalled registry, otherwise never returns at all.
    const abort = new AbortController();
    const deadline = setTimeout(() => abort.abort(), BUILD_TIMEOUT_MS);
    deadline.unref();
    try {
    await new Promise<void>((resolve, reject) => {
        const child = spawn("docker", args, {
            stdio: ["ignore", "ignore", "pipe"],
            signal: abort.signal,
            killSignal: "SIGKILL",
        });
        const tail: string[] = [];
        let pending = "";
        child.stderr.on("data", (chunk: Buffer) => {
            pending += chunk.toString("utf8");
            const parts = pending.split("\n");
            pending = parts.pop() ?? "";
            for (const line of parts) {
                tail.push(line);
                if (tail.length > BUILD_STDERR_TAIL_LINES) tail.shift();
            }
        });
        child.on("error", (e) => {
            reject(
                abort.signal.aborted
                    ? new Error(`docker build exceeded ${BUILD_TIMEOUT_MS}ms and was killed`)
                    : e,
            );
        });
        child.on("exit", (code) => {
            if (code === 0) {
                resolve();
                return;
            }
            if (pending.length > 0) {
                tail.push(pending);
                if (tail.length > BUILD_STDERR_TAIL_LINES) tail.shift();
            }
            const captured = tail.length > 0 ? `\nlast ${tail.length} stderr lines:\n${tail.join("\n")}` : "";
            reject(new Error(`docker build exited ${code}${captured}`));
        });
    });
    } finally {
        clearTimeout(deadline);
    }
    log(`image ${tag} built`);
}

// Hard ceiling on how stale a cached baybo-browser:* image we'll silently
// fall back to. Past this, the Chrome inside is likely missing a quarter
// of security fixes; refusing the fallback forces the operator to either
// fix the build (network/disk/apt) or explicitly opt into the old image
// via browser.docker.image_tag.
const STALE_IMAGE_MAX_AGE_DAYS = 30;

/**
 * Find the most recently created `baybo-browser:*` image present locally.
 * Used as a fallback when the deterministic-tag image isn't built and the
 * Chrome stable resolver couldn't reach the network — better to run a
 * slightly-stale cached image than to fall back to host-headless.
 *
 * Returns the tag along with its age in days so the caller can refuse
 * silently-old images and surface a loud warning when an image is on
 * the older end of the acceptable window.
 */
async function findCachedBayboBrowserImage(): Promise<
    { tag: string; ageDays: number } | undefined
> {
    let stdout: string;
    try {
        const r = await exec(
            "docker",
            [
                "images",
                "baybo-browser",
                "--format",
                "{{.Repository}}:{{.Tag}}\t{{.CreatedAt}}",
            ],
            { timeout: 5000 },
        );
        stdout = r.stdout;
    } catch {
        return undefined;
    }
    const lines = stdout
        .split("\n")
        .map((l) => l.trim())
        .filter((l) => l.length > 0 && !l.startsWith("baybo-browser:<none>"));
    // `docker images` already returns rows ordered most-recent first.
    const first = lines[0];
    if (first === undefined) return undefined;
    const [tag, createdAt] = first.split("\t");
    if (tag === undefined || createdAt === undefined || createdAt.length === 0) return undefined;
    const ageDays = parseDockerCreatedAtAgeDays(createdAt);
    if (ageDays === undefined) {
        log(`cached image ${tag} CreatedAt='${createdAt}' unparseable; assuming stale`);
        return undefined;
    }
    return { tag, ageDays };
}

/**
 * `docker images --format {{.CreatedAt}}` returns strings like
 * `2026-04-15 14:20:45 +0800 CST`. Date.parse can't handle the trailing
 * timezone abbreviation, so strip it before parsing.
 */
function parseDockerCreatedAtAgeDays(createdAt: string): number | undefined {
    const trimmed = createdAt.replace(/\s+[A-Z]{2,5}$/, "").trim();
    const ts = Date.parse(trimmed);
    if (Number.isNaN(ts)) return undefined;
    const ageMs = Date.now() - ts;
    if (ageMs < 0) return 0;
    return ageMs / (1000 * 60 * 60 * 24);
}

async function resolveImageTag(opts: DockerSpawnOptions): Promise<string> {
    if (opts.imageTag !== undefined) {
        log(`using operator-supplied image tag ${opts.imageTag}; skipping build`);
        return opts.imageTag;
    }

    const tag = await computeImageTag(opts.dockerDir);
    opts.onPhase("docker-building-image");
    if (await imageExists(tag)) {
        logDebug(`image ${tag} already cached`);
        return tag;
    }
    if (opts.neverBuild === true) {
        // The watchdog's repair path. A build here is minutes of work on the
        // one code path that must stay responsive, and it is the wrong place
        // for it: a first build belongs to boot. Failing fast hands the
        // problem to the escalation ladder, which re-boots the sidecar — where
        // building is legitimate and the transport is not yet anyone's
        // dependency.
        throw new Error(
            `image ${tag} is not present and this is a recovery spawn, which does not build. ` +
                `Restarting the sidecar will rebuild it.`,
        );
    }
    try {
        await buildImage(opts.dockerDir, tag);
        return tag;
    } catch (e) {
        const reason = e instanceof Error ? e.message : String(e);
        log(`build of ${tag} failed (${reason}); looking for a cached baybo-browser image`);
        const cached = await findCachedBayboBrowserImage();
        if (cached && cached.ageDays <= STALE_IMAGE_MAX_AGE_DAYS) {
            const ageStr = cached.ageDays.toFixed(1);
            log(
                `WARNING: build failed; falling back to cached image ${cached.tag} ` +
                    `(${ageStr} days old, contains a Chrome that may be missing recent ` +
                    `security fixes). Fix the build (network/apt/disk) or pin a known image ` +
                    `via baybo.json:browser.docker.image_tag.`,
            );
            return cached.tag;
        }
        if (cached) {
            const ageStr = cached.ageDays.toFixed(1);
            throw new Error(
                `image build failed and the only cached baybo-browser:* image (${cached.tag}) is ` +
                    `${ageStr} days old (cap is ${STALE_IMAGE_MAX_AGE_DAYS}d). Refusing the silent ` +
                    `fallback to a likely-vulnerable Chrome — fix the build, or set ` +
                    `baybo.json:browser.docker.image_tag to opt into the old image explicitly. ` +
                    `Build error: ${reason}`,
            );
        }
        throw new Error(
            `image build failed and no cached baybo-browser:* image found locally: ${reason}`,
        );
    }
}

export async function spawnContainer(opts: DockerSpawnOptions): Promise<DockerHandle> {
    // `:` is the field separator in `docker -v src:dst[:opts]`; a path
    // containing it would either silently re-interpret the suffix as a
    // mount option or break parsing entirely. Validate before we get
    // anywhere near the docker CLI so the operator gets a clear message
    // instead of a `Mounts denied` / `invalid mode` error from docker.
    rejectColonInBindPath(opts.profileDir, "browser.profile_dir");
    if (opts.fontDir !== undefined) {
        rejectColonInBindPath(opts.fontDir, "<workspace>/work/.fonts (font bind-mount source)");
    }
    if (opts.workDir !== undefined) {
        rejectColonInBindPath(opts.workDir, "<workspace>/work (work bind-mount source)");
    }

    const tag = await resolveImageTag(opts);

    opts.onPhase("docker-starting-container");
    // mkdirSync({recursive:true}) is idempotent — pre-checking existence
    // is a TOCTOU smell with no benefit.
    const { mkdirSync } = await import("node:fs");
    mkdirSync(opts.profileDir, { recursive: true });

    const containerName = `baybo-browser-${process.pid}-${randomBytes(3).toString("hex")}`;
    const viewport = `${opts.viewport.width}x${opts.viewport.height}`;
    // NOT `(... ) || 1000`: uid 0 is falsy, so a gateway running as root used to
    // publish `--user 1000:1000` and write the bind-mounted profile as the wrong
    // owner. The fallback is only for a platform without getuid at all.
    const uid = typeof process.getuid === "function" ? process.getuid() : 1000;
    const gid = typeof process.getgid === "function" ? process.getgid() : 1000;

    // We deliberately do NOT pass `--rm` here. If Chrome (or anything in
    // the entrypoint) crashes on startup, an `--rm` container deletes
    // itself before we can fetch logs, surfacing as a useless
    // "Container logs: (no logs)" diagnostic. Without --rm the container
    // sits in `Exited` state until we explicitly `docker rm` it, and
    // `docker logs` works fine. Cleanup is owned by `stop()` (success
    // path), the catch block below (post-run failure path), the watchdog's
    // `recover()` (which replaces an unresponsive container), and the
    // next-boot `sweepStaleContainers()` (kill -9 the gateway path). Every
    // one of those owes a `describeDeadContainer` first — removing the
    // container is what makes these logs unreachable.
    const runArgs = [
        "run",
        "-d",
        "--name",
        containerName,
        "--label",
        "baybo.role=browser-sidecar",
        "--label",
        `baybo.pid=${process.pid}`,
        "--user",
        `${uid}:${gid}`,
        "--shm-size=2g",
        `--pids-limit=${CONTAINER_PIDS_LIMIT}`,
        "-v",
        `${opts.profileDir}:${CONTAINER_PROFILE_DIR}`,
        "-e",
        `VIEWPORT=${viewport}`,
        "-e",
        `DISK_CACHE_SIZE=${opts.diskCacheBytes}`,
        // Container port 9223 is the socat relay (entrypoint.sh) bound to
        // 0.0.0.0, which forwards to Chrome's loopback-only 127.0.0.1:9222.
        // Chrome 134+ silently ignores `--remote-debugging-address=0.0.0.0`
        // and only accepts CDP from the loopback interface — the relay is
        // what makes published ports reachable.
        "-p",
        "127.0.0.1::9223",
    ];
    // Chrome's `localhost` is this container, which is the single most
    // confusing thing about docker mode. Publishing the bridge gateway
    // under docker's conventional name gives the agent a name that
    // resolves *correctly* — and, on a host whose DNS is rewritten by a
    // proxy, an /etc/hosts entry is what stops that name from silently
    // resolving to a decoy that accepts connections and returns nothing.
    if (opts.hostGatewayAlias === true) {
        runArgs.push("--add-host", `${HOST_GATEWAY_ALIAS}:host-gateway`);
    }
    if (opts.fontDir !== undefined) {
        runArgs.push("-v", `${opts.fontDir}:${CONTAINER_FONTS_DIR}:ro`);
    }
    // Read-only on purpose. The agent needs to *view* what it just wrote
    // (an HTML report, a rendered chart); it has a filesystem tool for
    // writing. Read-only also keeps page JS from reaching back into the
    // workspace through a `file://` origin.
    if (opts.workDir !== undefined) {
        runArgs.push("-v", `${opts.workDir}:${CONTAINER_WORK_DIR}:ro`);
    }
    // Without a limit the container inherits the host's memory. Chrome
    // never gives a tab back, so "unbounded" is the operative word: the
    // ceiling is whatever the host has, and the host OOM killer picks the
    // victim — possibly the gateway.
    //
    // What a cap buys is containment, NOT automatic recovery. The cgroup
    // OOM killer picks within the container, and Chrome renderers carry
    // `oom_score_adj=300` so a tab dies first; the browser process and the
    // CDP relay survive, which means the watchdog's `/json/version` probe
    // still answers and `recover()` never fires. The agent sees a dead tab
    // (a page that stops responding), the host sees nothing. Detecting
    // renderer death would need a CDP `Target.targetCrashed` subscription,
    // which the watchdog does not have.
    //
    // `--memory-swap` equal to `--memory` disables swap for the container
    // on purpose: swapping a browser trades an honest OOM for minutes of
    // thrash that looks like a hang.
    if (opts.memoryLimit !== undefined) {
        runArgs.push(`--memory=${opts.memoryLimit}`, `--memory-swap=${opts.memoryLimit}`);
    }
    // Browser-based VNC observability via noVNC + websockify on
    // webVncPort. x11vnc runs on a fixed internal :5900 inside the
    // container and websockify proxies to it; only the WEB port is
    // ever published to the host.
    if (opts.webVncPort !== undefined) {
        runArgs.push(
            "-e",
            `WEB_VNC_PORT=${opts.webVncPort}`,
            "-p",
            `127.0.0.1:${opts.webVncPort}:${opts.webVncPort}`,
        );
    }
    runArgs.push(tag);

    try {
        await exec("docker", runArgs, { timeout: 30_000 });
    } catch (e) {
        throw new Error(`docker run failed: ${(e as Error).message}`);
    }
    logDebug(`container ${containerName} started; resolving published CDP port`);

    // Anything that fails after `docker run` succeeded must force-remove
    // the container before propagating: otherwise the caller's
    // host-headless fallback would launch a second Chrome against the
    // same `--user-data-dir`, producing profile lock contention and
    // potential corruption (Chrome's profile lock is process-scoped, not
    // host-scoped, so the in-container Chrome doesn't block the host
    // launcher). `--rm` only triggers on a clean stop; a hung Chrome or
    // a network-unreachable port leaves the container running until the
    // next-boot sweep.
    try {
        const cdpUrl = await readPublishedCdpUrl(containerName);
        logDebug(`container CDP at ${cdpUrl}; waiting for Chrome to come up`);

        opts.onPhase("docker-waiting-for-cdp");
        await waitForCdp(cdpUrl, containerName);

        const stop = async (): Promise<void> => {
            try {
                await exec("docker", ["stop", "-t", "5", containerName], { timeout: 10_000 });
            } catch {
                /* best-effort */
            }
            try {
                await exec("docker", ["rm", "-f", containerName], { timeout: 10_000 });
            } catch {
                /* best-effort; sweep handles leaks */
            }
        };

        return { cdpUrl, containerName, imageTag: tag, stop };
    } catch (e) {
        try {
            await exec("docker", ["rm", "-f", containerName], { timeout: 10_000 });
            log(`removed failed container ${containerName} during error recovery`);
        } catch (cleanupErr) {
            log(
                `WARNING: docker rm -f ${containerName} failed during error recovery ` +
                    `(${(cleanupErr as Error).message}); container may still be running and ` +
                    `holding the Chrome profile. Run \`docker rm -f ${containerName}\` manually ` +
                    `before relaunching the gateway.`,
            );
        }
        throw e;
    }
}

async function readPublishedCdpUrl(containerName: string): Promise<string> {
    // 9223 is the socat-relay port, not Chrome's native 9222 — see the
    // `--remote-debugging-port` workaround in entrypoint.sh.
    const { stdout } = await exec("docker", ["port", containerName, "9223/tcp"], {
        timeout: 5000,
    });
    // `docker port` prints lines like `0.0.0.0:32773` and `[::]:32773`.
    // Pick the IPv4 binding; that's what we'll connect to via 127.0.0.1.
    const lines = stdout
        .split("\n")
        .map((l) => l.trim())
        .filter((l) => l.length > 0);
    const v4 = lines.find((l) => /^\d+\.\d+\.\d+\.\d+:\d+$/.test(l));
    if (v4 === undefined) {
        throw new Error(`docker port returned no IPv4 mapping (got: ${stdout.trim()})`);
    }
    const port = v4.split(":").pop();
    return `http://127.0.0.1:${port}`;
}

/**
 * One CDP liveness probe.
 *
 * `/json/version` is the cheapest thing Chrome's DevTools HTTP interface
 * serves and needs no CDP session, so it can run on an idle timer without
 * touching chrome-devtools-mcp's tool mutex — which is what makes it usable
 * as the watchdog's steady-state health check, not just a boot gate.
 *
 * Resolves when Chrome answers; throws with a short reason otherwise.
 */
export async function probeCdpEndpoint(
    cdpUrl: string,
    signal?: AbortSignal,
): Promise<void> {
    const res = await fetch(`${cdpUrl}/json/version`, signal !== undefined ? { signal } : {});
    if (!res.ok) {
        throw new Error(`HTTP ${res.status}`);
    }
}

/** Per-attempt ceiling inside {@link waitForCdp}. */
const CDP_PROBE_TIMEOUT_MS = 2_000;

async function waitForCdp(cdpUrl: string, containerName: string): Promise<void> {
    const deadline = Date.now() + 30_000;
    let lastErr = "(no probe yet)";
    let containerExitedEarly = false;
    let pollCount = 0;
    while (Date.now() < deadline) {
        try {
            // Without a signal this inherits undici's ~300s header timeout, so the
            // 30s budget below would silently become five minutes against a
            // container that accepts the connection and then never replies.
            await probeCdpEndpoint(cdpUrl, AbortSignal.timeout(CDP_PROBE_TIMEOUT_MS));
            return;
        } catch (e) {
            lastErr = (e as Error).message;
        }
        // Container-state probe via `docker inspect` — fast but each call
        // forks a subprocess. Throttle to every ~1s so a 1-3s normal Chrome
        // boot doesn't spawn 4-12 inspects when the fetch retries are doing
        // the same work. Still gives us ~1s detection latency on container
        // crash, fast enough to bail out of the 30s wait.
        pollCount += 1;
        if (pollCount % 4 === 0) {
            const status = await containerStatus(containerName);
            if (status !== undefined && status !== "running" && status !== "created") {
                containerExitedEarly = true;
                lastErr = `container exited early (state=${status})`;
                break;
            }
        }
        await sleep(250);
    }
    const logs = await fetchContainerLogs(containerName);
    const exitInfo = await containerExitInfo(containerName);
    const earlyTag = containerExitedEarly ? " [container exited]" : "";
    throw new Error(
        `Chrome CDP at ${cdpUrl} did not come up${earlyTag} (last error: ${lastErr}).\n` +
            `Container exit: ${exitInfo}\n` +
            `Container logs (last 60 lines):\n${logs}`,
    );
}

async function containerStatus(containerName: string): Promise<string | undefined> {
    try {
        const r = await exec(
            "docker",
            ["inspect", "--format", "{{.State.Status}}", containerName],
            { timeout: 5000 },
        );
        return r.stdout.trim();
    } catch {
        return undefined;
    }
}

async function containerExitInfo(containerName: string): Promise<string> {
    try {
        const r = await exec(
            "docker",
            [
                "inspect",
                "--format",
                "status={{.State.Status}} exit={{.State.ExitCode}} oom={{.State.OOMKilled}} error={{.State.Error}}",
                containerName,
            ],
            { timeout: 5000 },
        );
        return r.stdout.trim() || "(no inspect output)";
    } catch (e) {
        return `inspect failed: ${(e as Error).message}`;
    }
}

function rejectColonInBindPath(path: string, label: string): void {
    if (path.includes(":")) {
        throw new Error(
            `${label} contains ':' (${path}); docker -v parses 'src:dst[:opts]' so the colon would ` +
                `corrupt the bind-mount. Move the directory to a path without ':' before enabling docker mode.`,
        );
    }
}

/** Log lines kept in a post-mortem. Smaller than the boot-failure dump: this
 * one can fire up to once per recovery attempt. */
const POST_MORTEM_TAIL_LINES = 40;

/**
 * Post-mortem for a container that is about to be removed.
 *
 * `docker run` deliberately omits `--rm` so a crashed container's logs stay
 * fetchable — see the rationale in {@link spawnContainer}. Anything that
 * removes a container therefore owes the operator this first, or the recovery
 * that fixes the symptom also destroys the only evidence of the cause: an OOM
 * that will recur every few minutes reads as bad luck instead of a container
 * that needs more memory.
 *
 * Returned as one string so the caller emits one NDJSON line — the gateway's
 * stderr drain budgets by line, and embedded newlines survive the round trip.
 */
export async function describeDeadContainer(containerName: string): Promise<string> {
    const [exit, logs] = await Promise.all([
        containerExitInfo(containerName),
        fetchContainerLogs(containerName, POST_MORTEM_TAIL_LINES),
    ]);
    return `exit: ${exit}\nlast ${POST_MORTEM_TAIL_LINES} log lines:\n${logs}`;
}

async function fetchContainerLogs(
    containerName: string,
    tailLines = 60,
): Promise<string> {
    try {
        // Without --rm on the container, logs are still attached even
        // after the container exits, so this works in both the
        // hung-but-alive and crashed-and-stopped cases.
        const r = await exec("docker", ["logs", "--tail", String(tailLines), containerName], {
            timeout: 5000,
        });
        const combined = `${r.stdout}${r.stderr}`.trim();
        return combined.length > 0 ? combined : "(empty — entrypoint produced no output)";
    } catch (e) {
        return `(docker logs failed: ${(e as Error).message})`;
    }
}
