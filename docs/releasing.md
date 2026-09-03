# Cutting a release

A release is a git tag, a GitHub Release, two Linux binaries, and `install.sh`.
It is **not** a crates.io publish (blocked — every internal dep is a bare
`{ path = … }` with no version, and `crates/gateway` path-depends across the
workspace boundary into the excluded `remote-host/` tree, so `cargo package`
fails before it reaches the network), not an npm publish (every package is
`private: true`), and not an App Store submission (`app/ios/scripts/release.mjs`
is a separate, Mac-local lane).

Three targets ship: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` and
`aarch64-apple-darwin`. Intel Macs do not — they can only be cross-built from an
arm64 runner and never smoke-tested, and shipping an artifact nobody has
executed is worse than shipping none.

The darwin leg was proven on real hardware before it was wired up (Apple M4,
macOS 26.5): a full `BAYBO_REQUIRE_SIDECARS=1` release build finishes in **5m37s**
and produces a working `Mach-O 64-bit executable arm64`, with the real dashboard
and all three sidecar bundles embedded. Two things worth recording because they
were the scary unknowns and both turned out fine: `aws-lc-sys` builds without
`cmake` installed at all, and `strip` does not break execution — Apple's `strip`
re-signs the binary ad-hoc on the spot (the code-signing identifier changes and
it still runs), so no explicit `codesign` step is needed. `release-build.sh`
re-checks that by executing the stripped binary anyway, because if it were ever
wrong the symptom is SIGKILL with no message at all.

Note the build is not very parallel — 864s of CPU over 5m37s wall is an average
of 2.7 cores — so the 3-CPU `macos-26` runner should land near the same figure
rather than 3x it.

## Cutting one

**1. Open a version-bump PR.** Four files move together, and CI will reject the
PR if any of them lags:

```bash
# The one canonical version.
$EDITOR Cargo.toml                     # [workspace.package] version = "0.2.0"

# Two tracked lockfiles carry it. app/ios is its own cargo workspace but
# path-depends on crates/{wire,device-proto,model}, so it moves too.
# remote-host/ has its own version and does NOT move with this one.
cargo update -w
(cd app/ios && cargo update -w)

# utoipa defaults info.version to CARGO_PKG_VERSION, and openapi_json_is_in_sync
# byte-compares the committed spec.
UPDATE_OPENAPI=1 cargo test -p baybo-gateway --test all openapi_json_is_in_sync
```

That PR also drags the iOS jobs along, because `app/ios/Cargo.lock` matches both
`IOS_DEPS` and `ios_native` in `ci.yml` — one `ios-sim` run on `macos-26`, which
takes one of the five macOS slots the free plan allows account-wide. Measured at
~21 runner-minutes for the whole PR. Merge it once green.

**2. Dispatch the release.** Actions → Release → Run workflow, from `master`.
Tick `dry_run` first if you want to prove the build before publishing anything;
a dry run does everything except create the release.

That is the whole ritual. The workflow reads the version out of `Cargo.toml`,
refuses if that tag already exists, builds both targets, smoke-tests each
artifact, publishes, and then installs the published release in three distro
containers to prove `install.sh` still agrees with the asset names.

`workflow_dispatch` only offers workflows that exist on the **default branch**,
so `release.yml` cannot be run — or even rehearsed — until it is merged to
master. The first dispatch is therefore also its first execution.

**If something goes wrong.** A failed build leaves no tag and no release, so fix
and re-dispatch. A failure *after* publishing is self-healing in both places it
can happen: the publish step drops the tag it just pushed, and `verify-install`
deletes the release and its tag outright, on the grounds that a live release
`install.sh` cannot consume is worse than no release. Either way the gate will
accept the same version again. Use **Re-run failed jobs** rather than *Re-run
all jobs* when you can — both work, but the former skips a 25-minute rebuild.

## What the workflow does, and the three traps it is shaped around

Read the header of [`.github/workflows/release.yml`](../.github/workflows/release.yml)
for the full reasoning. In short:

- **The tag is created last, in the publish job, and by a plain `git push`.**
  Last, because a tag pushed by a workflow using `GITHUB_TOKEN` raises no
  events — so the conventional "tag here, build on the tag over there" split is
  silently dead, the tag lands and nothing ever builds — and because tagging
  after the builds means a run that dies at minute 40 leaves nothing to clean
  up. A plain push rather than `gh release create --target <sha>`, because
  GitHub demands a `workflow` scope for a release whose target SHA both touches
  `.github/workflows/` and has no ref of its own, and `workflow` is not a
  grantable key in a `permissions:` block. Master moves ~10 times a day, so the
  dispatched SHA routinely loses its ref during a build; pushing the tag gives
  it a permanent one.
- **The binaries are built in `manylinux_2_28`, not on the runner.** The glibc
  floor is a property of the machine that links the binary and is invisible to
  every gate this repo has — it fails in the dynamic loader, before `main`, so
  no panic hook or log line ever sees it. Building on `ubuntu-latest` yields a
  floor of 2.39, which fails to load on RHEL 9, Amazon Linux 2023, Debian 12 and
  Ubuntu 22.04. `rust:bookworm` looks like the careful choice and is worse than
  it appears: 2.36 still excludes the whole 2.34 cluster. 2.28 loads everywhere
  tested. The `redhat/ubi9` smoke step exists to catch a regression here.
- **The toolchain that builds the binary is pinned, and nothing is piped from a
  URL into a shell.** The build images carry a digest, not a tag — v0.1.0 was
  built inside an image whose `latest` had been re-pointed twenty minutes
  earlier, which meant a registry chose the compiler of a binary strangers pipe
  into their shells. `NODE_VERSION` is a full `x.y.z` for the same reason: it
  used to be a major that the build resolved against nodejs.org at run time, so
  two builds of one commit were not the same build. node and bun are now
  downloaded and checked against the `SHASUMS256.txt` each project publishes
  beside the artifact. Be honest about what that buys: the digest travels from
  the same origin over the same TLS session, so it catches corruption and a
  tampered mirror, not a compromised upstream. rustup is left alone on purpose —
  its channel manifest already hashes every component it installs.

- **The workflow writes nothing to the Actions cache.** The repo already sits at
  roughly 13 GB against a 10 GB per-repo budget and is evicting continuously; a
  release-profile entry would evict the ones that keep every PR's `test` job
  warm. Every release is therefore cold, which is the right trade for something
  that runs a handful of times a year.

Two further things the release build does that no other job does. It compiles
**without** `BAYBO_SKIP_WEBUI` (both Rust jobs in `ci.yml` set it, so the
dashboard-baked binary has never otherwise been built in CI) and **with**
`BAYBO_REQUIRE_SIDECARS=1`. The second is necessary but not sufficient: it makes
a discovered sidecar failure fatal, yet `build.rs` still treats a missing
entrypoint as a bare warning and an unreadable `sidecars/` directory as an empty
asset table. `scripts/release-build.sh` therefore asserts the three bundles and
the real dashboard are present after the build, because neither failure shows up
in the binary's size — the two channel bundles are about 0.18% of it.

## install.sh

Served from the repo (`raw.githubusercontent.com/.../master/install.sh`) and
also attached to each release. It refuses rather than guesses: musl, glibc older
than 2.28, an Intel Mac, an unknown architecture, and a box without `git` all
get a written explanation instead of a 404 or a binary that dies on first run.
On Apple silicon it consults `sysctl.proc_translated` rather than `uname -m`,
which lies under Rosetta and would otherwise hand an M-series user the Intel
build that does not exist.

The `git` gate is not caution. `ensure_layout` `git init`s three identity repos
during startup for every subcommand but a handful, so without git the first
command a new user types fails with a raw `spawn 'git init …'` error. Everything
else — `rg`, `bun`, `node`, `uv`, a sandbox backend — degrades *silently*, often
with the only warning going to `~/.baybo/logs/`, so `install.sh` names each one
and says exactly what breaks.

It deliberately stops short of `baybo setup` (interactive; needs a TTY on both
stdin and stderr, which a pipe cannot provide) and of `baybo gateway install`
(`resolve_service_path` bakes the *installing* process's PATH into the unit, and
inside `curl | sh` — before the rc edit takes effect — `~/.local/bin` is not on
PATH yet, so the daemon would permanently lose `bun`/`node`/`uv`).

The `git` check mirrors `crates/process/src/host_tool.rs` for `bun` and `node`
specifically: those two resolve through an env override, then PATH, then
`~/.local/bin`, then `~/.bun/bin`, so checking PATH alone reports a missing bun
on machines where the daemon finds it (bun's installer puts it in `~/.bun/bin`
and only adds that to PATH for login shells). `rg` and `uv` really are
PATH-only, so a plain lookup is right for them.

Test it locally with `scripts/test-install.sh`, which serves a fake release from
localhost and installs it inside Debian 12, Ubuntu 22.04 and UBI 9, and asserts
the musl and no-git refusals. The loop is about half a second per iteration once
the images are baked; CI runs the same script whenever one of these files
changes. The darwin leg cannot be containerised — verify it by hand on a Mac,
which is also how the two bugs above were found.

**Why the advertised URL is a mutable branch, and what pays for it.** Users
fetch `install.sh` from **master** but get assets from the **latest release**,
so the two drift between releases. That pairing is ordinary — a survey of 41
projects found 18 of the 33 that ship an installer serve mutable
default-branch bytes, Homebrew included, and `cargo-binstall` uses baybo's exact
shape (raw-branch script, `releases/latest/download/` assets, a `--version`
pin). What separates the projects that survive it (chezmoi, k3s) from the ones
that merely get away with it (cargo-binstall, zoxide, starship, none of which
verify anything) is not where the script is hosted: it is that the script
verifies the payload against a digest published under the **same release**.
That is why the checksum gate above fails closed rather than warning.

The alternative — advertising
`github.com/booiris/baybo/releases/latest/download/install.sh`, which every
release already uploads — is worth revisiting once releases exist. It is not
better today: with no release published, that URL 404s and `curl -fsSL … | sh`
pipes nothing to a shell and **exits 0**, a silent no-op, where the raw URL
degrades to the "no releases yet, build from source" message this script writes
for exactly that case. The same asymmetry returns every time `verify-install`
unpublishes a bad release. If it is ever switched, do it as the last step of a
release that has already gone green, not as part of a merge.

Every published target is verified against the real release before it is allowed
to stand: `verify-install` installs into three Linux containers (one of them
through the unpinned `/releases/latest/` path) and `verify-install-macos`
installs natively on `macos-26` and asserts the binary is a `Mach-O 64-bit
executable arm64`. If either fails, `delist-unverified` marks the release a
prerelease, which takes it out of `/releases/latest/download/`.

It de-lists rather than deletes, and that is a deliberate reversal of the
original design. The trigger cannot tell "install.sh is broken" from "Docker Hub
rate-limited us", and deletion paid out by destroying the assets, the notes, the
download counts and — via `--cleanup-tag` — the only ref pinning the shipped
commit. De-listing removes the entire user-facing harm and leaves everything a
human needs to diagnose it; `gh release edit <tag> --prerelease=false --latest`
undoes it. Reserve deletion for a person who has looked. The verifiers also
carry `timeout-minutes` (they would otherwise inherit the 6-hour default and act
on a release long after users had it) and retry their container runs three
times, because pulling images anonymously from Docker Hub and running apt/dnf
against public mirrors is the flakiest thing in the workflow.

The job is one, not a step in each verifier, because two verifiers racing on the
same release would leave the loser red for no reason. It is guarded on
`needs.release.result == 'success'` — an `if:` overrides the usual
skip-with-your-dependency rule, so a build failure would otherwise reach it with
no release to act on — and on `!cancelled()` rather than `failure()`, since a
cancelled verifier also leaves a release that nothing confirmed.

One gap remains: `--version <old-tag>` runs today's installer against an old
release's assets, so asset naming is frozen forever once a release is public.

**The one string no language can share:** the asset name
(`baybo-<triple>.tar.gz`) lives in `scripts/release-build.sh` and in
`install.sh`. Rename it on one side and every user gets a 404 while CI stays
green. The `verify-install` job at the end of the release workflow is the only
thing that ever proves the two still agree — do not remove it.
