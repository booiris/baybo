// Docker-spawn helpers for the browser sidecar.
//
// Lifecycle:
//   1. checkDockerAvailable() — `docker info` with a 3s timeout. Returns
//      `{ ok:false }` (with a reason string) when the daemon is missing,
//      down, or unreachable. Caller falls back to host-headless.
//   2. sweepStaleContainers() — list `aura.role=browser-sidecar` labelled
//      containers, drop any whose `aura.pid` label points at a dead
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
// `$XDG_CACHE_HOME/aura/browser/profile` round-trips cleanly between
// host-headless and docker modes. Without this, switching modes leaves
// the operator unable to read their own profile.
//
// Why `-p 127.0.0.1::9222`: ephemeral host-side port chosen by docker
// (read back via `docker port`), bound to loopback only. Multiple Aura
// instances on one host don't collide; CDP isn't reachable from off-box.

import { execFile, spawn } from "node:child_process";
import { promisify } from "node:util";
import { createHash, randomBytes } from "node:crypto";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";

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
    webVncPort?: number;
    viewport: { width: number; height: number };
    onPhase: (p: DockerPhase) => void;
}

export interface DockerHandle {
    cdpUrl: string;
    containerName: string;
    imageTag: string;
    stop: () => Promise<void>;
}

const log = (msg: string): void => {
    process.stderr.write(`[browser-mcp:docker] ${msg}\n`);
};

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
                "label=aura.role=browser-sidecar",
                "--format",
                "{{.Names}}\t{{.Label \"aura.pid\"}}",
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
        if (!name) continue;
        // `Number(pidStr)` returns NaN on partial parse; `parseInt("12abc",10)`
        // would silently truncate to 12 and falsely treat the container's
        // owner as still-alive when the label is corrupted.
        const pid = pidStr ? Number(pidStr) : NaN;
        if (Number.isFinite(pid) && pid > 0 && isPidAlive(pid)) {
            continue;
        }
        log(`sweep: removing stale container ${name} (owner pid=${pidStr || "?"} not alive)`);
        try {
            await exec("docker", ["rm", "-f", name], { timeout: 5000 });
        } catch (e) {
            log(`sweep: docker rm -f ${name} failed: ${(e as Error).message}`);
        }
    }
}

export function isPidAlive(pid: number): boolean {
    try {
        process.kill(pid, 0);
        return true;
    } catch {
        return false;
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
    return `aura-browser:${hash.digest("hex").slice(0, 12)}`;
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
    // Stream stderr so the operator sees apt-get progress in real time
    // rather than a 3-minute silent wait.
    await new Promise<void>((resolve, reject) => {
        const child = spawn("docker", args, { stdio: ["ignore", "ignore", "inherit"] });
        child.on("error", reject);
        child.on("exit", (code) => {
            if (code === 0) resolve();
            else reject(new Error(`docker build exited ${code}`));
        });
    });
    log(`image ${tag} built`);
}

// Hard ceiling on how stale a cached aura-browser:* image we'll silently
// fall back to. Past this, the Chrome inside is likely missing a quarter
// of security fixes; refusing the fallback forces the operator to either
// fix the build (network/disk/apt) or explicitly opt into the old image
// via browser.docker.image_tag.
const STALE_IMAGE_MAX_AGE_DAYS = 30;

/**
 * Find the most recently created `aura-browser:*` image present locally.
 * Used as a fallback when the deterministic-tag image isn't built and the
 * Chrome stable resolver couldn't reach the network — better to run a
 * slightly-stale cached image than to fall back to host-headless.
 *
 * Returns the tag along with its age in days so the caller can refuse
 * silently-old images and surface a loud warning when an image is on
 * the older end of the acceptable window.
 */
async function findCachedAuraBrowserImage(): Promise<
    { tag: string; ageDays: number } | undefined
> {
    let stdout: string;
    try {
        const r = await exec(
            "docker",
            [
                "images",
                "aura-browser",
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
        .filter((l) => l.length > 0 && !l.startsWith("aura-browser:<none>"));
    // `docker images` already returns rows ordered most-recent first.
    const first = lines[0];
    if (!first) return undefined;
    const [tag, createdAt] = first.split("\t");
    if (!tag || !createdAt) return undefined;
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
    if (opts.imageTag) {
        log(`using operator-supplied image tag ${opts.imageTag}; skipping build`);
        return opts.imageTag;
    }

    const tag = await computeImageTag(opts.dockerDir);
    opts.onPhase("docker-building-image");
    if (await imageExists(tag)) {
        log(`image ${tag} already cached`);
        return tag;
    }
    try {
        await buildImage(opts.dockerDir, tag);
        return tag;
    } catch (e) {
        const reason = e instanceof Error ? e.message : String(e);
        log(`build of ${tag} failed (${reason}); looking for a cached aura-browser image`);
        const cached = await findCachedAuraBrowserImage();
        if (cached && cached.ageDays <= STALE_IMAGE_MAX_AGE_DAYS) {
            const ageStr = cached.ageDays.toFixed(1);
            log(
                `WARNING: build failed; falling back to cached image ${cached.tag} ` +
                    `(${ageStr} days old, contains a Chrome that may be missing recent ` +
                    `security fixes). Fix the build (network/apt/disk) or pin a known image ` +
                    `via aura.json:browser.docker.image_tag.`,
            );
            return cached.tag;
        }
        if (cached) {
            const ageStr = cached.ageDays.toFixed(1);
            throw new Error(
                `image build failed and the only cached aura-browser:* image (${cached.tag}) is ` +
                    `${ageStr} days old (cap is ${STALE_IMAGE_MAX_AGE_DAYS}d). Refusing the silent ` +
                    `fallback to a likely-vulnerable Chrome — fix the build, or set ` +
                    `aura.json:browser.docker.image_tag to opt into the old image explicitly. ` +
                    `Build error: ${reason}`,
            );
        }
        throw new Error(
            `image build failed and no cached aura-browser:* image found locally: ${reason}`,
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
    if (opts.fontDir) {
        rejectColonInBindPath(opts.fontDir, "<workspace>/work/.fonts (font bind-mount source)");
    }

    const tag = await resolveImageTag(opts);

    opts.onPhase("docker-starting-container");
    // mkdirSync({recursive:true}) is idempotent — pre-checking existence
    // is a TOCTOU smell with no benefit.
    const { mkdirSync } = await import("node:fs");
    mkdirSync(opts.profileDir, { recursive: true });

    const containerName = `aura-browser-${process.pid}-${randomBytes(3).toString("hex")}`;
    const viewport = `${opts.viewport.width}x${opts.viewport.height}`;
    const uid = (typeof process.getuid === "function" ? process.getuid() : 1000) || 1000;
    const gid = (typeof process.getgid === "function" ? process.getgid() : 1000) || 1000;

    // We deliberately do NOT pass `--rm` here. If Chrome (or anything in
    // the entrypoint) crashes on startup, an `--rm` container deletes
    // itself before we can fetch logs, surfacing as a useless
    // "Container logs: (no logs)" diagnostic. Without --rm the container
    // sits in `Exited` state until we explicitly `docker rm` it, and
    // `docker logs` works fine. Cleanup is owned by `stop()` (success
    // path), the catch block below (post-run failure path), and the
    // next-boot `sweepStaleContainers()` (kill -9 the gateway path).
    const runArgs = [
        "run",
        "-d",
        "--name",
        containerName,
        "--label",
        "aura.role=browser-sidecar",
        "--label",
        `aura.pid=${process.pid}`,
        "--user",
        `${uid}:${gid}`,
        "--shm-size=2g",
        "-v",
        `${opts.profileDir}:/data/profile`,
        "-e",
        `VIEWPORT=${viewport}`,
        // Container port 9223 is the socat relay (entrypoint.sh) bound to
        // 0.0.0.0, which forwards to Chrome's loopback-only 127.0.0.1:9222.
        // Chrome 134+ silently ignores `--remote-debugging-address=0.0.0.0`
        // and only accepts CDP from the loopback interface — the relay is
        // what makes published ports reachable.
        "-p",
        "127.0.0.1::9223",
    ];
    if (opts.fontDir) {
        runArgs.push("-v", `${opts.fontDir}:/data/fonts:ro`);
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
    log(`container ${containerName} started; resolving published CDP port`);

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
        log(`container CDP at ${cdpUrl}; waiting for Chrome to come up`);

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
    if (!v4) {
        throw new Error(`docker port returned no IPv4 mapping (got: ${stdout.trim()})`);
    }
    const port = v4.split(":").pop();
    return `http://127.0.0.1:${port}`;
}

async function waitForCdp(cdpUrl: string, containerName: string): Promise<void> {
    const deadline = Date.now() + 30_000;
    let lastErr = "(no probe yet)";
    let containerExitedEarly = false;
    let pollCount = 0;
    while (Date.now() < deadline) {
        try {
            const res = await fetch(`${cdpUrl}/json/version`);
            if (res.ok) return;
            lastErr = `HTTP ${res.status}`;
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
            if (status && status !== "running" && status !== "created") {
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

async function fetchContainerLogs(containerName: string): Promise<string> {
    try {
        // Without --rm on the container, logs are still attached even
        // after the container exits, so this works in both the
        // hung-but-alive and crashed-and-stopped cases.
        const r = await exec("docker", ["logs", "--tail", "60", containerName], {
            timeout: 5000,
        });
        const combined = `${r.stdout}${r.stderr}`.trim();
        return combined.length > 0 ? combined : "(empty — entrypoint produced no output)";
    } catch (e) {
        return `(docker logs failed: ${(e as Error).message})`;
    }
}
