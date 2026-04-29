use std::ffi::OsString;
use std::path::Path;

use crate::WorkspaceSymlinkMount;
use crate::spec::{EnvPolicy, FilesystemPolicy, NetworkPolicy, SandboxSpec};

const BWRAP_RO_ROOTS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/etc",
    "/run/systemd/resolve",
];

pub fn build_bwrap_argv(
    spec: &SandboxSpec,
    workspace_symlink_mount: Option<&WorkspaceSymlinkMount>,
) -> Vec<OsString> {
    let mut argv: Vec<OsString> = Vec::with_capacity(64);

    let push = |a: &mut Vec<OsString>, s: &str| a.push(OsString::from(s));

    push(&mut argv, "--die-with-parent");
    push(&mut argv, "--new-session");
    push(&mut argv, "--clearenv");
    push(&mut argv, "--unshare-user");
    push(&mut argv, "--unshare-pid");
    push(&mut argv, "--unshare-uts");
    push(&mut argv, "--unshare-ipc");
    push(&mut argv, "--unshare-cgroup-try");
    match spec.network_policy {
        NetworkPolicy::None => push(&mut argv, "--unshare-net"),
        NetworkPolicy::All => push(&mut argv, "--share-net"),
    }

    match &spec.filesystem_policy {
        FilesystemPolicy::Workspace => {
            push(&mut argv, "--proc");
            push(&mut argv, "/proc");
            push(&mut argv, "--dev");
            push(&mut argv, "/dev");
            push(&mut argv, "--tmpfs");
            push(&mut argv, "/tmp");

            for root in BWRAP_RO_ROOTS {
                argv.push(OsString::from("--ro-bind-try"));
                argv.push(OsString::from(*root));
                argv.push(OsString::from(*root));
            }

            argv.push(OsString::from("--bind"));
            argv.push(spec.workspace_root.as_os_str().to_owned());
            argv.push(spec.workspace_root.as_os_str().to_owned());

            // Mount a validated symlink spelling of the requested cwd
            // so the sandbox can chdir to the path the caller supplied.
            // bwrap creates the intermediate directories on the tmpfs
            // root automatically.
            if let Some(mount) = workspace_symlink_mount {
                argv.push(OsString::from("--bind"));
                argv.push(mount.source().as_os_str().to_owned());
                argv.push(mount.destination().as_os_str().to_owned());
            }

            for path in &spec.readable_paths {
                argv.push(OsString::from("--ro-bind-try"));
                argv.push(path.as_os_str().to_owned());
                argv.push(path.as_os_str().to_owned());
            }

            for path in &spec.writable_paths {
                argv.push(OsString::from("--bind"));
                argv.push(path.as_os_str().to_owned());
                argv.push(path.as_os_str().to_owned());
            }
        }
        FilesystemPolicy::Permissive {
            extra_root,
            denied_paths,
        } => {
            push(&mut argv, "--proc");
            push(&mut argv, "/proc");
            push(&mut argv, "--dev");
            push(&mut argv, "/dev");
            push(&mut argv, "--tmpfs");
            push(&mut argv, "/tmp");

            // FHS roots stay RO so installed binaries and resolver
            // config still work. `--ro-bind-try` is forgiving on hosts
            // where one of these doesn't exist (e.g. `/lib64` on some
            // Debian flavours).
            for root in BWRAP_RO_ROOTS {
                argv.push(OsString::from("--ro-bind-try"));
                argv.push(OsString::from(*root));
                argv.push(OsString::from(*root));
            }

            // workspace_root is always RW so the project the agent was
            // launched in stays writable even when it lives outside
            // `extra_root` (e.g. `/data/proj` while $HOME is
            // `/home/u`). bwrap accepts overlapping binds; if the
            // workspace already sits inside extra_root, the second
            // bind is a redundant no-op.
            argv.push(OsString::from("--bind"));
            argv.push(spec.workspace_root.as_os_str().to_owned());
            argv.push(spec.workspace_root.as_os_str().to_owned());

            argv.push(OsString::from("--bind-try"));
            argv.push(extra_root.as_os_str().to_owned());
            argv.push(extra_root.as_os_str().to_owned());

            // Each denied path becomes an empty per-call tmpfs that
            // hides the host directory underneath. Non-existent paths
            // are silently skipped at the SandboxAdapter layer so
            // bwrap never sees a `--tmpfs <missing>` line.
            for path in denied_paths {
                argv.push(OsString::from("--tmpfs"));
                argv.push(path.as_os_str().to_owned());
            }
        }
    }

    for (k, v) in resolve_env(spec) {
        argv.push(OsString::from("--setenv"));
        argv.push(OsString::from(k));
        argv.push(OsString::from(v));
    }

    let chdir = spec.cwd.as_deref().unwrap_or(spec.workspace_root.as_path());
    argv.push(OsString::from("--chdir"));
    argv.push(chdir.as_os_str().to_owned());

    argv.push(OsString::from("--"));
    argv.push(spec.program.as_os_str().to_owned());
    for a in &spec.args {
        argv.push(OsString::from(a));
    }

    argv
}

pub(crate) fn resolve_env(spec: &SandboxSpec) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let workspace = spec.workspace_root.display().to_string();
    out.push(("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into()));
    out.push(("HOME".into(), workspace));
    // Point temp env vars at the bwrap-mounted tmpfs `/tmp` so callers
    // that respect $TMPDIR write into the disposable per-sandbox tmpfs
    // instead of polluting the workspace bind.
    out.push(("TMPDIR".into(), "/tmp".into()));
    out.push(("TMP".into(), "/tmp".into()));
    out.push(("TEMP".into(), "/tmp".into()));
    for opt in ["LANG", "LC_ALL", "TZ"] {
        if let Ok(v) = std::env::var(opt) {
            out.push((opt.into(), v));
        }
    }
    if let EnvPolicy::Allowlist { vars } = &spec.env {
        for k in vars {
            if let Ok(v) = std::env::var(k) {
                out.push((k.clone(), v));
            }
        }
    }
    out.push(("PWD".into(), effective_pwd(spec)));
    out
}

pub(crate) fn effective_pwd(spec: &SandboxSpec) -> String {
    spec.cwd
        .as_deref()
        .unwrap_or(spec.workspace_root.as_path())
        .display()
        .to_string()
}

/// Build the `docker run …` argv for the cross-platform fallback runner.
///
/// The mapping intentionally mirrors bwrap semantics so the same
/// `SandboxSpec` produces equivalent observable behavior:
///
/// - workspace bind RW with the same path inside and outside the
///   container (so absolute paths in the host and container line up
///   for the `cwd` validation in the agent adapter and for any path
///   the tool emits in its output);
/// - `--network none` for `NetworkPolicy::None`, default bridge for
///   `NetworkPolicy::All` — `host` networking is not portable to
///   macOS Docker Desktop, so we deliberately avoid it;
/// - `--user host_uid:host_gid` so files the child writes back into
///   the workspace bind are owned by the host user rather than root;
/// - `$TMPDIR` / `$TMP` / `$TEMP` route at the in-container `/tmp`
///   tmpfs (every container gets its own);
/// - `--name <container_name>` so the runner can `docker rm -f` the
///   daemon-side container on timeout/cancellation rather than
///   leaving it running;
/// - `--pull=never` so a missing or upstream-rotated image surfaces
///   loudly instead of silently swapping the trusted execution base
///   between calls — `DockerRunner::warm()` is responsible for
///   pulling and pinning the image up front.
pub fn build_docker_argv(
    spec: &SandboxSpec,
    workspace_symlink_mount: Option<&WorkspaceSymlinkMount>,
    image: &str,
    container_name: &str,
) -> Vec<OsString> {
    let mut argv: Vec<OsString> = Vec::with_capacity(64);
    argv.push(OsString::from("run"));
    argv.push(OsString::from("--rm"));
    argv.push(OsString::from("--init"));
    argv.push(OsString::from("--pull=never"));
    argv.push(OsString::from("--name"));
    argv.push(OsString::from(container_name));

    match spec.network_policy {
        NetworkPolicy::None => {
            argv.push(OsString::from("--network"));
            argv.push(OsString::from("none"));
        }
        NetworkPolicy::All => {
            argv.push(OsString::from("--network"));
            argv.push(OsString::from("bridge"));
        }
    }

    // Drop linux capabilities the child should not have. `--init` already
    // covers PID-1 reaping, so we just need to keep the kernel-level
    // surface narrow.
    argv.push(OsString::from("--cap-drop=ALL"));

    if let Some(bytes) = spec.resource_limits.memory_max_bytes {
        argv.push(OsString::from("--memory"));
        argv.push(OsString::from(format!("{bytes}")));
    }
    if let Some(pids) = spec.resource_limits.pids_max {
        argv.push(OsString::from("--pids-limit"));
        argv.push(OsString::from(format!("{pids}")));
    }

    let (uid, gid) = current_uid_gid();
    argv.push(OsString::from("--user"));
    argv.push(OsString::from(format!("{uid}:{gid}")));

    let ws = spec.workspace_root.as_os_str().to_owned();
    argv.push(OsString::from("-v"));
    let mut bind = ws.clone();
    bind.push(":");
    bind.push(&ws);
    argv.push(bind);

    // Same as bwrap: mount a validated symlink spelling of the
    // requested cwd so Docker can chdir to the path the caller supplied.
    if let Some(mount) = workspace_symlink_mount {
        argv.push(OsString::from("-v"));
        let mut symlink_bind = mount.source().as_os_str().to_owned();
        symlink_bind.push(":");
        symlink_bind.push(mount.destination().as_os_str());
        argv.push(symlink_bind);
    }

    for path in &spec.readable_paths {
        let p = path.as_os_str().to_owned();
        argv.push(OsString::from("-v"));
        let mut ro = p.clone();
        ro.push(":");
        ro.push(&p);
        ro.push(":ro");
        argv.push(ro);
    }

    for path in &spec.writable_paths {
        let p = path.as_os_str().to_owned();
        argv.push(OsString::from("-v"));
        let mut rw = p.clone();
        rw.push(":");
        rw.push(&p);
        argv.push(rw);
    }

    let cwd = spec.cwd.as_deref().unwrap_or(spec.workspace_root.as_path());
    argv.push(OsString::from("-w"));
    argv.push(cwd.as_os_str().to_owned());

    for (k, v) in resolve_env(spec) {
        argv.push(OsString::from("-e"));
        argv.push(OsString::from(format!("{k}={v}")));
    }

    if matches!(
        spec.stdin,
        crate::spec::StdinSource::Bytes(_) | crate::spec::StdinSource::Inherit
    ) {
        argv.push(OsString::from("-i"));
    }

    argv.push(OsString::from(image));
    argv.push(spec.program.as_os_str().to_owned());
    for a in &spec.args {
        argv.push(OsString::from(a));
    }

    argv
}

fn current_uid_gid() -> (u32, u32) {
    // Safety: `getuid` and `getgid` are signal-safe and cannot fail per POSIX.
    unsafe { (libc::getuid(), libc::getgid()) }
}

pub fn render_sbpl_profile(
    spec: &SandboxSpec,
    workspace_symlink_mount: Option<&WorkspaceSymlinkMount>,
) -> String {
    let mut s = String::new();
    s.push_str("(version 1)\n");
    s.push_str("(deny default)\n");
    s.push_str("(deny file-write* (subpath \"/\"))\n\n");

    s.push_str("(allow process-fork)\n");
    s.push_str("(allow process-exec)\n");
    s.push_str("(allow signal (target self))\n\n");

    match &spec.filesystem_policy {
        FilesystemPolicy::Workspace => {
            s.push_str("(allow file-read*\n");
            for p in [
                "/usr",
                "/System",
                "/Library",
                "/private/var/db/dyld",
                "/private/etc",
                "/bin",
                "/sbin",
                "/private/tmp",
            ] {
                s.push_str(&format!("  (subpath \"{}\")\n", sbpl_quote(Path::new(p))));
            }
            s.push_str(&format!(
                "  (subpath \"{}\")\n",
                sbpl_quote(&spec.workspace_root)
            ));
            if let Some(mount) = workspace_symlink_mount {
                s.push_str(&format!(
                    "  (subpath \"{}\")\n",
                    sbpl_quote(mount.destination())
                ));
            }
            for p in &spec.readable_paths {
                s.push_str(&format!("  (subpath \"{}\")\n", sbpl_quote(p)));
            }
            for p in &spec.writable_paths {
                s.push_str(&format!("  (subpath \"{}\")\n", sbpl_quote(p)));
            }
            s.push_str(")\n\n");

            // Workspace is the only host path the child may write to.
            // Host temp directories (`/private/tmp`,
            // `/private/var/folders`) are deliberately excluded — the
            // macOS runner sets `TMPDIR` to a per-invocation scratch
            // dir under the workspace so anything respecting `$TMPDIR`
            // lands in the workspace bind instead of leaking onto the
            // host.
            s.push_str("(allow file-write*\n");
            s.push_str(&format!(
                "  (subpath \"{}\")\n",
                sbpl_quote(&spec.workspace_root)
            ));
            if let Some(mount) = workspace_symlink_mount {
                s.push_str(&format!(
                    "  (subpath \"{}\")\n",
                    sbpl_quote(mount.destination())
                ));
            }
            for p in &spec.writable_paths {
                s.push_str(&format!("  (subpath \"{}\")\n", sbpl_quote(p)));
            }
            s.push_str(")\n\n");
        }
        FilesystemPolicy::Permissive {
            extra_root,
            denied_paths,
        } => {
            // Allow read on the FHS-mac equivalent + workspace +
            // extra_root; allow write on workspace + extra_root only.
            // Then deny read+write on each sensitive subpath via
            // last-match-wins so credential vaults stay opaque even
            // though they sit under `extra_root`.
            s.push_str("(allow file-read*\n");
            for p in [
                "/usr",
                "/System",
                "/Library",
                "/private/var/db/dyld",
                "/private/etc",
                "/bin",
                "/sbin",
            ] {
                s.push_str(&format!("  (subpath \"{}\")\n", sbpl_quote(Path::new(p))));
            }
            s.push_str(&format!(
                "  (subpath \"{}\")\n",
                sbpl_quote(&spec.workspace_root)
            ));
            s.push_str(&format!("  (subpath \"{}\")\n", sbpl_quote(extra_root)));
            if let Some(mount) = workspace_symlink_mount {
                s.push_str(&format!(
                    "  (subpath \"{}\")\n",
                    sbpl_quote(mount.destination())
                ));
            }
            s.push_str(")\n\n");

            s.push_str("(allow file-write*\n");
            s.push_str(&format!(
                "  (subpath \"{}\")\n",
                sbpl_quote(&spec.workspace_root)
            ));
            s.push_str(&format!("  (subpath \"{}\")\n", sbpl_quote(extra_root)));
            if let Some(mount) = workspace_symlink_mount {
                s.push_str(&format!(
                    "  (subpath \"{}\")\n",
                    sbpl_quote(mount.destination())
                ));
            }
            s.push_str(")\n\n");

            for p in denied_paths {
                s.push_str(&format!(
                    "(deny file-read* (subpath \"{}\"))\n",
                    sbpl_quote(p)
                ));
                s.push_str(&format!(
                    "(deny file-write* (subpath \"{}\"))\n",
                    sbpl_quote(p)
                ));
            }
            s.push('\n');
        }
    }

    s.push_str("(allow mach-lookup)\n");
    s.push_str("(allow ipc-posix-shm)\n");
    s.push_str("(allow sysctl-read)\n\n");

    match (spec.network_policy, spec.allowed_hosts.is_empty()) {
        (NetworkPolicy::None, _) => s.push_str("(deny network*)\n"),
        (NetworkPolicy::All, true) => s.push_str("(allow network*)\n"),
        (NetworkPolicy::All, false) => {
            // Per-host scope. The kernel does name resolution before
            // matching the `(remote tcp "host:port")` rule, so we must
            // separately allow DNS (both UDP 53 and TCP 53 for edge
            // cases like fallback to DoT or large responses) for the
            // hostname rules to be useful at all. Everything else is
            // denied by the leading `network*` rule.
            s.push_str("(deny network*)\n");
            s.push_str("(allow network-outbound (remote udp \"*:53\"))\n");
            s.push_str("(allow network-outbound (remote tcp \"*:53\"))\n");
            for host in &spec.allowed_hosts {
                let entry = if host.contains(':') {
                    host.clone()
                } else {
                    // Bare hostname → any port. Operators who want to
                    // pin a port encode it as `host:443`.
                    format!("{host}:*")
                };
                s.push_str(&format!(
                    "(allow network-outbound (remote tcp \"{}\"))\n",
                    sbpl_quote_str(&entry)
                ));
            }
        }
    }
    s
}

pub fn sbpl_quote(path: &Path) -> String {
    sbpl_quote_str(&path.display().to_string())
}

pub fn sbpl_quote_str(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{EnvPolicy, StdinSource};
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::time::Duration;

    fn spec_for(network: NetworkPolicy, ws: &str) -> SandboxSpec {
        SandboxSpec {
            program: PathBuf::from("sh"),
            args: vec!["-c".into(), "echo hi".into()],
            cwd: None,
            workspace_root: PathBuf::from(ws),
            readable_paths: vec![],
            writable_paths: vec![],
            allowed_hosts: BTreeSet::new(),
            network_policy: network,
            env: EnvPolicy::Baseline,
            stdin: StdinSource::Null,
            timeout: Duration::from_secs(5),
            resource_limits: crate::spec::ResourceLimits::default(),
            filesystem_policy: FilesystemPolicy::default(),
        }
    }

    fn argv_strs(argv: &[OsString]) -> Vec<String> {
        argv.iter().map(|s| s.to_string_lossy().into()).collect()
    }

    #[test]
    fn bwrap_argv_unshares_net_when_no_http() {
        let argv = build_bwrap_argv(&spec_for(NetworkPolicy::None, "/tmp/ws"), None);
        let strs = argv_strs(&argv);
        assert!(strs.contains(&"--unshare-net".to_string()));
        assert!(!strs.contains(&"--share-net".to_string()));
    }

    #[test]
    fn bwrap_argv_shares_net_for_http() {
        let argv = build_bwrap_argv(&spec_for(NetworkPolicy::All, "/tmp/ws"), None);
        let strs = argv_strs(&argv);
        assert!(strs.contains(&"--share-net".to_string()));
        assert!(!strs.contains(&"--unshare-net".to_string()));
    }

    #[test]
    fn bwrap_argv_binds_requested_symlink_path_to_canonical_source() {
        let spec = spec_for(NetworkPolicy::None, "/data/proj/.aura/work");
        let symlink_mount = WorkspaceSymlinkMount::new(
            PathBuf::from("/data/proj/.aura/work"),
            PathBuf::from("/home/u/proj/.aura/work"),
        );
        let argv = build_bwrap_argv(&spec, Some(&symlink_mount));
        let strs = argv_strs(&argv);
        assert!(
            strs.windows(3).any(|w| w[0] == "--bind"
                && w[1] == "/data/proj/.aura/work"
                && w[2] == "/data/proj/.aura/work"),
            "primary workspace bind must be present: {strs:?}"
        );
        assert!(
            strs.windows(3).any(|w| w[0] == "--bind"
                && w[1] == "/data/proj/.aura/work"
                && w[2] == "/home/u/proj/.aura/work"),
            "expected --bind /data/proj/.aura/work /home/u/proj/.aura/work in argv: {strs:?}"
        );
    }

    #[test]
    fn docker_argv_binds_requested_symlink_path_to_canonical_source() {
        let spec = spec_for(NetworkPolicy::None, "/data/proj/.aura/work");
        let symlink_mount = WorkspaceSymlinkMount::new(
            PathBuf::from("/data/proj/.aura/work"),
            PathBuf::from("/home/u/proj/.aura/work"),
        );
        let argv = build_docker_argv(
            &spec,
            Some(&symlink_mount),
            "debian:stable-slim",
            "aura-sandbox-test",
        );
        let strs = argv_strs(&argv);
        assert!(
            strs.iter()
                .any(|a| a == "/data/proj/.aura/work:/data/proj/.aura/work"),
            "primary -v bind missing: {strs:?}"
        );
        assert!(
            strs.iter()
                .any(|a| a == "/data/proj/.aura/work:/home/u/proj/.aura/work"),
            "symlink path -v bind missing: {strs:?}"
        );
    }

    #[test]
    fn sbpl_profile_grants_requested_symlink_path_read_and_write() {
        let spec = spec_for(NetworkPolicy::None, "/data/proj/.aura/work");
        let symlink_mount = WorkspaceSymlinkMount::new(
            PathBuf::from("/data/proj/.aura/work"),
            PathBuf::from("/home/u/proj/.aura/work"),
        );
        let s = render_sbpl_profile(&spec, Some(&symlink_mount));
        let read_start = s
            .find("(allow file-read*")
            .expect("file-read* block must exist");
        let read_end_rel = s[read_start..]
            .find(")\n\n")
            .expect("file-read* terminator");
        let read_block = &s[read_start..read_start + read_end_rel];
        assert!(
            read_block.contains("(subpath \"/home/u/proj/.aura/work\")"),
            "requested symlink path must be readable: {read_block}"
        );
        let write_start = s
            .find("(allow file-write*")
            .expect("file-write* block must exist");
        let write_end_rel = s[write_start..]
            .find(")\n\n")
            .expect("file-write* terminator");
        let write_block = &s[write_start..write_start + write_end_rel];
        assert!(
            write_block.contains("(subpath \"/home/u/proj/.aura/work\")"),
            "requested symlink path must be writable: {write_block}"
        );
    }

    #[test]
    fn bwrap_argv_permissive_binds_workspace_and_extra_root_rw_and_masks_denied_paths() {
        // Pick workspace_root OUTSIDE extra_root so we exercise the
        // case where the project lives outside $HOME (e.g.
        // `/data/proj` while $HOME is `/home/agent`).
        let mut spec = spec_for(NetworkPolicy::All, "/data/proj");
        spec.filesystem_policy = FilesystemPolicy::Permissive {
            extra_root: PathBuf::from("/home/agent"),
            denied_paths: vec![
                PathBuf::from("/home/agent/.ssh"),
                PathBuf::from("/home/agent/.aws"),
            ],
        };
        let argv = build_bwrap_argv(&spec, None);
        let strs = argv_strs(&argv);

        // No host-root bind. Workspace and extra_root each get an
        // explicit RW bind; extra_root uses --bind-try because $HOME
        // can be missing on minimal containers / CI environments.
        assert!(
            !strs
                .windows(3)
                .any(|w| w[0] == "--bind" && w[1] == "/" && w[2] == "/"),
            "permissive mode must NOT bind host root: {strs:?}"
        );
        assert!(
            strs.windows(3)
                .any(|w| w[0] == "--bind" && w[1] == "/data/proj" && w[2] == "/data/proj"),
            "workspace must be RW-bound: {strs:?}"
        );
        assert!(
            strs.windows(3)
                .any(|w| w[0] == "--bind-try" && w[1] == "/home/agent" && w[2] == "/home/agent"),
            "extra_root ($HOME) must be RW-bound (try): {strs:?}"
        );

        for denied in ["/home/agent/.ssh", "/home/agent/.aws"] {
            assert!(
                strs.windows(2).any(|w| w[0] == "--tmpfs" && w[1] == denied),
                "denied path {denied} must be masked by tmpfs: {strs:?}"
            );
        }

        // FHS roots stay RO-bound so binaries / resolver config work.
        for ro in ["/usr", "/bin", "/etc"] {
            assert!(
                strs.windows(2)
                    .any(|w| w[0] == "--ro-bind-try" && w[1] == ro),
                "permissive mode must keep {ro} RO-bound: {strs:?}"
            );
        }

        // --share-net still respected via NetworkPolicy.
        assert!(strs.contains(&"--share-net".to_string()));
    }

    #[test]
    fn bwrap_argv_workspace_default_path_unchanged_when_permissive_off() {
        // Sanity: the existing strict-workspace argv shape is preserved
        // when filesystem_policy stays at its default Workspace variant.
        let argv = build_bwrap_argv(&spec_for(NetworkPolicy::None, "/tmp/ws"), None);
        let strs = argv_strs(&argv);
        assert!(
            strs.windows(2)
                .any(|w| w[0] == "--ro-bind-try" && w[1] == "/usr"),
            "workspace mode must keep /usr RO bind: {strs:?}"
        );
        assert!(
            !strs
                .windows(3)
                .any(|w| w[0] == "--bind" && w[1] == "/" && w[2] == "/"),
            "workspace mode must NOT bind host root: {strs:?}"
        );
    }

    #[test]
    fn bwrap_argv_workspace_is_rw_bind() {
        let argv = build_bwrap_argv(&spec_for(NetworkPolicy::None, "/tmp/myws"), None);
        let strs = argv_strs(&argv);
        let mut iter = strs.iter().enumerate();
        let bind_idx = iter
            .find(|(_, a)| *a == "--bind")
            .expect("--bind for workspace")
            .0;
        assert_eq!(strs[bind_idx + 1], "/tmp/myws");
        assert_eq!(strs[bind_idx + 2], "/tmp/myws");
        assert!(!strs.iter().enumerate().any(
            |(i, a)| a == "--ro-bind-try" && strs.get(i + 1).is_some_and(|p| p == "/tmp/myws")
        ));
    }

    #[test]
    fn bwrap_argv_clearenv_precedes_setenv() {
        let mut spec = spec_for(NetworkPolicy::None, "/tmp/ws");
        spec.env = EnvPolicy::Allowlist {
            vars: vec!["PATH".into()],
        };
        let argv = build_bwrap_argv(&spec, None);
        let strs = argv_strs(&argv);
        let clearenv = strs.iter().position(|a| a == "--clearenv");
        let setenv = strs.iter().position(|a| a == "--setenv");
        assert!(clearenv.is_some(), "--clearenv must be present");
        assert!(setenv.is_some(), "--setenv must be present");
        assert!(clearenv.unwrap() < setenv.unwrap());
    }

    #[test]
    fn bwrap_argv_routes_tmpdir_to_internal_tmpfs() {
        let argv = build_bwrap_argv(&spec_for(NetworkPolicy::None, "/tmp/ws"), None);
        let strs = argv_strs(&argv);
        let positions: Vec<usize> = strs
            .iter()
            .enumerate()
            .filter_map(|(i, a)| {
                (a == "--setenv" && strs.get(i + 1).is_some_and(|k| k == "TMPDIR")).then_some(i)
            })
            .collect();
        assert_eq!(
            positions.len(),
            1,
            "expected exactly one TMPDIR setenv, got {positions:?}"
        );
        let i = positions[0];
        assert_eq!(strs[i + 2], "/tmp", "TMPDIR must point at the tmpfs");
    }

    #[test]
    fn resolve_env_sets_pwd_to_workspace_when_cwd_missing() {
        let spec = spec_for(NetworkPolicy::None, "/tmp/ws");
        let env = resolve_env(&spec);
        assert_eq!(
            env.iter()
                .find_map(|(k, v)| (k == "PWD").then_some(v.as_str())),
            Some("/tmp/ws")
        );
    }

    #[test]
    fn resolve_env_sets_pwd_to_explicit_cwd() {
        let mut spec = spec_for(NetworkPolicy::None, "/tmp/ws");
        spec.cwd = Some(PathBuf::from("/tmp/ws/subdir"));
        let env = resolve_env(&spec);
        assert_eq!(
            env.iter()
                .find_map(|(k, v)| (k == "PWD").then_some(v.as_str())),
            Some("/tmp/ws/subdir")
        );
    }

    #[test]
    fn bwrap_argv_terminates_args_with_double_dash() {
        let argv = build_bwrap_argv(&spec_for(NetworkPolicy::None, "/tmp/ws"), None);
        let strs = argv_strs(&argv);
        let dd = strs.iter().position(|a| a == "--").expect("`--` separator");
        assert_eq!(strs[dd + 1], "sh");
        assert_eq!(strs[dd + 2], "-c");
        assert_eq!(strs[dd + 3], "echo hi");
    }

    #[test]
    fn sbpl_profile_starts_with_default_deny() {
        let s = render_sbpl_profile(&spec_for(NetworkPolicy::None, "/tmp/ws"), None);
        let mut lines = s.lines();
        assert_eq!(lines.next(), Some("(version 1)"));
        assert_eq!(lines.next(), Some("(deny default)"));
    }

    #[test]
    fn sbpl_profile_network_off_for_no_http() {
        let s = render_sbpl_profile(&spec_for(NetworkPolicy::None, "/tmp/ws"), None);
        assert!(s.contains("(deny network*)"));
        assert!(!s.contains("(allow network*)"));
    }

    #[test]
    fn sbpl_profile_network_on_for_http() {
        let s = render_sbpl_profile(&spec_for(NetworkPolicy::All, "/tmp/ws"), None);
        assert!(s.contains("(allow network*)"));
        assert!(!s.contains("(deny network*)"));
    }

    #[test]
    fn sbpl_profile_writes_only_to_workspace() {
        let s = render_sbpl_profile(&spec_for(NetworkPolicy::None, "/tmp/myws"), None);
        // Find the `(allow file-write*` block and confirm it lists only
        // the workspace path, no host temp directories.
        let write_block_start = s
            .find("(allow file-write*")
            .expect("profile must allow workspace writes");
        let after = &s[write_block_start..];
        let write_block_end = after.find(")\n\n").expect("write block terminator");
        let block = &after[..write_block_end];
        assert!(
            block.contains("(subpath \"/tmp/myws\")"),
            "workspace must be in write block: {block}"
        );
        assert!(
            !block.contains("/private/tmp"),
            "v1 must not allow writes to host /private/tmp: {block}"
        );
        assert!(
            !block.contains("/private/var/folders"),
            "v1 must not allow writes to host /private/var/folders: {block}"
        );
    }

    #[test]
    fn docker_argv_starts_with_run_rm_init() {
        let argv = build_docker_argv(
            &spec_for(NetworkPolicy::None, "/tmp/myws"),
            None,
            "debian:stable-slim",
            "aura-sandbox-test",
        );
        let strs = argv_strs(&argv);
        assert_eq!(strs[0], "run");
        assert_eq!(strs[1], "--rm");
        assert_eq!(strs[2], "--init");
    }

    #[test]
    fn docker_argv_disables_network_when_no_http() {
        let argv = build_docker_argv(
            &spec_for(NetworkPolicy::None, "/tmp/myws"),
            None,
            "debian:stable-slim",
            "aura-sandbox-test",
        );
        let strs = argv_strs(&argv);
        let i = strs
            .iter()
            .position(|a| a == "--network")
            .expect("--network present");
        assert_eq!(strs[i + 1], "none");
    }

    #[test]
    fn docker_argv_uses_bridge_network_for_http() {
        let argv = build_docker_argv(
            &spec_for(NetworkPolicy::All, "/tmp/myws"),
            None,
            "debian:stable-slim",
            "aura-sandbox-test",
        );
        let strs = argv_strs(&argv);
        let i = strs
            .iter()
            .position(|a| a == "--network")
            .expect("--network present");
        assert_eq!(strs[i + 1], "bridge");
    }

    #[test]
    fn docker_argv_binds_workspace_with_matching_paths() {
        let argv = build_docker_argv(
            &spec_for(NetworkPolicy::None, "/tmp/myws"),
            None,
            "debian:stable-slim",
            "aura-sandbox-test",
        );
        let strs = argv_strs(&argv);
        let i = strs.iter().position(|a| a == "-v").expect("-v present");
        assert_eq!(strs[i + 1], "/tmp/myws:/tmp/myws");
    }

    #[test]
    fn docker_argv_exports_pwd_to_workspace() {
        let argv = build_docker_argv(
            &spec_for(NetworkPolicy::None, "/tmp/myws"),
            None,
            "debian:stable-slim",
            "aura-sandbox-test",
        );
        let strs = argv_strs(&argv);
        assert!(
            strs.windows(2)
                .any(|w| w[0] == "-e" && w[1] == "PWD=/tmp/myws"),
            "expected docker env PWD=/tmp/myws in argv: {strs:?}"
        );
    }

    #[test]
    fn docker_argv_drops_capabilities_and_pins_user() {
        let argv = build_docker_argv(
            &spec_for(NetworkPolicy::None, "/tmp/myws"),
            None,
            "debian:stable-slim",
            "aura-sandbox-test",
        );
        let strs = argv_strs(&argv);
        assert!(strs.contains(&"--cap-drop=ALL".to_string()));
        let i = strs
            .iter()
            .position(|a| a == "--user")
            .expect("--user present");
        let pinned = &strs[i + 1];
        assert!(
            pinned.contains(':')
                && pinned
                    .split(':')
                    .all(|seg| seg.chars().all(|c| c.is_ascii_digit())),
            "expected uid:gid pair, got `{pinned}`"
        );
    }

    #[test]
    fn docker_argv_emits_memory_and_pids_limits_when_set() {
        let mut spec = spec_for(NetworkPolicy::None, "/tmp/myws");
        spec.resource_limits = crate::spec::ResourceLimits {
            memory_max_bytes: Some(256 * 1024 * 1024),
            pids_max: Some(64),
        };
        let argv = build_docker_argv(&spec, None, "debian:stable-slim", "aura-sandbox-test");
        let strs = argv_strs(&argv);
        let mem = strs
            .iter()
            .position(|a| a == "--memory")
            .expect("--memory present");
        assert_eq!(strs[mem + 1], (256u64 * 1024 * 1024).to_string());
        let pids = strs
            .iter()
            .position(|a| a == "--pids-limit")
            .expect("--pids-limit present");
        assert_eq!(strs[pids + 1], "64");
    }

    #[test]
    fn docker_argv_omits_caps_when_unlimited() {
        let mut spec = spec_for(NetworkPolicy::None, "/tmp/myws");
        spec.resource_limits = crate::spec::ResourceLimits::unlimited();
        let argv = build_docker_argv(&spec, None, "debian:stable-slim", "aura-sandbox-test");
        let strs = argv_strs(&argv);
        assert!(
            !strs.iter().any(|a| a == "--memory"),
            "no --memory when unlimited: {strs:?}"
        );
        assert!(
            !strs.iter().any(|a| a == "--pids-limit"),
            "no --pids-limit when unlimited: {strs:?}"
        );
    }

    #[test]
    fn docker_argv_image_precedes_program_and_args() {
        let argv = build_docker_argv(
            &spec_for(NetworkPolicy::None, "/tmp/myws"),
            None,
            "debian:stable-slim",
            "aura-sandbox-test",
        );
        let strs = argv_strs(&argv);
        let image_idx = strs
            .iter()
            .position(|a| a == "debian:stable-slim")
            .expect("image in argv");
        let program_idx = strs
            .iter()
            .rposition(|a| a == "sh")
            .expect("program in argv");
        assert!(
            image_idx < program_idx,
            "image must come before program (image@{image_idx}, program@{program_idx})"
        );
    }

    #[test]
    fn sbpl_profile_per_host_scopes_replace_broad_allow() {
        let mut spec = spec_for(NetworkPolicy::All, "/tmp/myws");
        spec.allowed_hosts = BTreeSet::from(["api.openai.com:443".into(), "github.com".into()]);
        let s = render_sbpl_profile(&spec, None);
        assert!(
            !s.contains("(allow network*)"),
            "broad allow must be dropped when allowed_hosts is non-empty: {s}"
        );
        assert!(s.contains("(deny network*)"));
        assert!(
            s.contains("(allow network-outbound (remote tcp \"api.openai.com:443\"))"),
            "explicit host:port entry must round-trip: {s}"
        );
        assert!(
            s.contains("(allow network-outbound (remote tcp \"github.com:*\"))"),
            "bare hostname must default to wildcard port: {s}"
        );
        assert!(
            s.contains("(allow network-outbound (remote udp \"*:53\"))"),
            "DNS must be allowed so hostname rules can resolve: {s}"
        );
    }

    #[test]
    fn sbpl_profile_allowed_hosts_ignored_under_network_none() {
        let mut spec = spec_for(NetworkPolicy::None, "/tmp/myws");
        spec.allowed_hosts = BTreeSet::from(["github.com".into()]);
        let s = render_sbpl_profile(&spec, None);
        assert!(s.contains("(deny network*)"));
        assert!(
            !s.contains("(allow network-outbound"),
            "allowed_hosts must not unblock egress when policy is None: {s}"
        );
    }

    #[test]
    fn bwrap_argv_writable_paths_are_rw_bound_not_ro_bound() {
        let mut spec = spec_for(NetworkPolicy::None, "/tmp/ws");
        spec.writable_paths = vec![PathBuf::from("/data/out"), PathBuf::from("/var/out2")];
        let argv = build_bwrap_argv(&spec, None);
        let strs = argv_strs(&argv);
        // Each entry must appear as `--bind <p> <p>` (RW), not as
        // `--ro-bind-try` (which is what readable_paths uses).
        for p in ["/data/out", "/var/out2"] {
            let bind_pos = strs
                .iter()
                .enumerate()
                .find(|(i, a)| {
                    *a == "--bind"
                        && strs.get(i + 1).map(String::as_str) == Some(p)
                        && strs.get(i + 2).map(String::as_str) == Some(p)
                })
                .map(|(i, _)| i);
            assert!(
                bind_pos.is_some(),
                "expected --bind {p} {p} in argv: {strs:?}"
            );
            assert!(
                !strs
                    .iter()
                    .enumerate()
                    .any(|(i, a)| a == "--ro-bind-try"
                        && strs.get(i + 1).map(String::as_str) == Some(p)),
                "writable path {p} must NOT appear under --ro-bind-try"
            );
        }
    }

    #[test]
    fn docker_argv_writable_paths_omit_ro_suffix() {
        let mut spec = spec_for(NetworkPolicy::None, "/tmp/myws");
        spec.writable_paths = vec![PathBuf::from("/data/out")];
        let argv = build_docker_argv(&spec, None, "debian:stable-slim", "aura-sandbox-test");
        let strs = argv_strs(&argv);
        // `-v <path>:<path>` for writable; `-v <path>:<path>:ro` for
        // readable. Confirm the writable entry uses the bare form.
        let writable_pos = strs
            .iter()
            .position(|a| a == "/data/out:/data/out")
            .expect("writable_paths must produce a bare RW bind");
        // The same path must NOT also appear with a `:ro` suffix.
        assert!(
            !strs.iter().any(|a| a == "/data/out:/data/out:ro"),
            "writable path must not also appear as :ro readonly: {strs:?}"
        );
        // And the `-v` flag immediately precedes it.
        assert!(
            writable_pos > 0 && strs[writable_pos - 1] == "-v",
            "expected -v to precede the bind spec at index {writable_pos}: {strs:?}"
        );
    }

    #[test]
    fn sbpl_writable_paths_extend_file_write_block() {
        let mut spec = spec_for(NetworkPolicy::None, "/tmp/myws");
        spec.writable_paths = vec![PathBuf::from("/data/out"), PathBuf::from("/var/log/app")];
        let s = render_sbpl_profile(&spec, None);
        // Locate the file-write* block boundaries.
        let start = s
            .find("(allow file-write*")
            .expect("profile must allow workspace writes");
        let after = &s[start..];
        let end_rel = after.find(")\n\n").expect("write block terminator");
        let block = &after[..end_rel];
        assert!(
            block.contains("(subpath \"/tmp/myws\")"),
            "workspace write entry preserved: {block}"
        );
        assert!(
            block.contains("(subpath \"/data/out\")"),
            "writable_paths entry must extend the write block: {block}"
        );
        assert!(
            block.contains("(subpath \"/var/log/app\")"),
            "second writable_paths entry must extend the write block: {block}"
        );
    }

    #[test]
    fn sbpl_writable_paths_also_grant_read_access() {
        // A writable subpath should also be readable — most file ops
        // (open, fstat) require both. Without this the script can
        // write but not read the path it just wrote.
        let mut spec = spec_for(NetworkPolicy::None, "/tmp/myws");
        spec.writable_paths = vec![PathBuf::from("/data/out")];
        let s = render_sbpl_profile(&spec, None);
        // Read block starts before the write block; verify the
        // writable path is listed in `(allow file-read* …)` too.
        let read_start = s
            .find("(allow file-read*")
            .expect("file-read* block must exist");
        let read_block_end = s[read_start..]
            .find(")\n\n")
            .expect("file-read* terminator");
        let read_block = &s[read_start..read_start + read_block_end];
        assert!(
            read_block.contains("(subpath \"/data/out\")"),
            "writable path must also be readable: {read_block}"
        );
    }

    #[test]
    fn sbpl_quote_str_escapes_quotes_and_backslashes() {
        assert_eq!(sbpl_quote_str(r#"a"b\c"#), r#"a\"b\\c"#);
    }

    #[test]
    fn sbpl_quote_escapes_quotes_and_backslashes() {
        let q = sbpl_quote(Path::new(r#"/tmp/with"quote\and\back"#));
        assert_eq!(q, r#"/tmp/with\"quote\\and\\back"#);
    }
}
