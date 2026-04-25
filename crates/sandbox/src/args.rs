use std::ffi::OsString;
use std::path::Path;

use crate::spec::{EnvPolicy, NetworkPolicy, SandboxSpec};

const BWRAP_RO_ROOTS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/etc",
    "/run/systemd/resolve",
];

pub fn build_bwrap_argv(spec: &SandboxSpec) -> Vec<OsString> {
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

    for path in &spec.readable_paths {
        argv.push(OsString::from("--ro-bind-try"));
        argv.push(path.as_os_str().to_owned());
        argv.push(path.as_os_str().to_owned());
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

fn resolve_env(spec: &SandboxSpec) -> Vec<(String, String)> {
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
    out
}

pub fn render_sbpl_profile(spec: &SandboxSpec) -> String {
    let mut s = String::new();
    s.push_str("(version 1)\n");
    s.push_str("(deny default)\n");
    s.push_str("(deny file-write* (subpath \"/\"))\n\n");

    s.push_str("(allow process-fork)\n");
    s.push_str("(allow process-exec)\n");
    s.push_str("(allow signal (target self))\n\n");

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
    for p in &spec.readable_paths {
        s.push_str(&format!("  (subpath \"{}\")\n", sbpl_quote(p)));
    }
    s.push_str(")\n\n");

    // Workspace is the only host path the child may write to. Host temp
    // directories (`/private/tmp`, `/private/var/folders`) are deliberately
    // excluded — the macOS runner sets `TMPDIR` to a per-invocation
    // scratch dir under the workspace so anything respecting `$TMPDIR`
    // lands in the workspace bind instead of leaking onto the host.
    s.push_str("(allow file-write*\n");
    s.push_str(&format!(
        "  (subpath \"{}\")\n",
        sbpl_quote(&spec.workspace_root)
    ));
    s.push_str(")\n\n");

    s.push_str("(allow mach-lookup)\n");
    s.push_str("(allow ipc-posix-shm)\n");
    s.push_str("(allow sysctl-read)\n\n");

    match spec.network_policy {
        NetworkPolicy::None => s.push_str("(deny network*)\n"),
        NetworkPolicy::All => s.push_str("(allow network*)\n"),
    }
    s
}

pub fn sbpl_quote(path: &Path) -> String {
    let raw = path.display().to_string();
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
            allowed_hosts: BTreeSet::new(),
            network_policy: network,
            env: EnvPolicy::Baseline,
            stdin: StdinSource::Null,
            timeout: Duration::from_secs(5),
        }
    }

    fn argv_strs(argv: &[OsString]) -> Vec<String> {
        argv.iter().map(|s| s.to_string_lossy().into()).collect()
    }

    #[test]
    fn bwrap_argv_unshares_net_when_no_http() {
        let argv = build_bwrap_argv(&spec_for(NetworkPolicy::None, "/tmp/ws"));
        let strs = argv_strs(&argv);
        assert!(strs.contains(&"--unshare-net".to_string()));
        assert!(!strs.contains(&"--share-net".to_string()));
    }

    #[test]
    fn bwrap_argv_shares_net_for_http() {
        let argv = build_bwrap_argv(&spec_for(NetworkPolicy::All, "/tmp/ws"));
        let strs = argv_strs(&argv);
        assert!(strs.contains(&"--share-net".to_string()));
        assert!(!strs.contains(&"--unshare-net".to_string()));
    }

    #[test]
    fn bwrap_argv_workspace_is_rw_bind() {
        let argv = build_bwrap_argv(&spec_for(NetworkPolicy::None, "/tmp/myws"));
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
        let argv = build_bwrap_argv(&spec);
        let strs = argv_strs(&argv);
        let clearenv = strs.iter().position(|a| a == "--clearenv");
        let setenv = strs.iter().position(|a| a == "--setenv");
        assert!(clearenv.is_some(), "--clearenv must be present");
        assert!(setenv.is_some(), "--setenv must be present");
        assert!(clearenv.unwrap() < setenv.unwrap());
    }

    #[test]
    fn bwrap_argv_routes_tmpdir_to_internal_tmpfs() {
        let argv = build_bwrap_argv(&spec_for(NetworkPolicy::None, "/tmp/ws"));
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
    fn bwrap_argv_terminates_args_with_double_dash() {
        let argv = build_bwrap_argv(&spec_for(NetworkPolicy::None, "/tmp/ws"));
        let strs = argv_strs(&argv);
        let dd = strs.iter().position(|a| a == "--").expect("`--` separator");
        assert_eq!(strs[dd + 1], "sh");
        assert_eq!(strs[dd + 2], "-c");
        assert_eq!(strs[dd + 3], "echo hi");
    }

    #[test]
    fn sbpl_profile_starts_with_default_deny() {
        let s = render_sbpl_profile(&spec_for(NetworkPolicy::None, "/tmp/ws"));
        let mut lines = s.lines();
        assert_eq!(lines.next(), Some("(version 1)"));
        assert_eq!(lines.next(), Some("(deny default)"));
    }

    #[test]
    fn sbpl_profile_network_off_for_no_http() {
        let s = render_sbpl_profile(&spec_for(NetworkPolicy::None, "/tmp/ws"));
        assert!(s.contains("(deny network*)"));
        assert!(!s.contains("(allow network*)"));
    }

    #[test]
    fn sbpl_profile_network_on_for_http() {
        let s = render_sbpl_profile(&spec_for(NetworkPolicy::All, "/tmp/ws"));
        assert!(s.contains("(allow network*)"));
        assert!(!s.contains("(deny network*)"));
    }

    #[test]
    fn sbpl_profile_writes_only_to_workspace() {
        let s = render_sbpl_profile(&spec_for(NetworkPolicy::None, "/tmp/myws"));
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
    fn sbpl_quote_escapes_quotes_and_backslashes() {
        let q = sbpl_quote(Path::new(r#"/tmp/with"quote\and\back"#));
        assert_eq!(q, r#"/tmp/with\"quote\\and\\back"#);
    }
}
