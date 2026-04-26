use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub workspace_root: PathBuf,
    pub readable_paths: Vec<PathBuf>,
    pub allowed_hosts: BTreeSet<String>,
    pub network_policy: NetworkPolicy,
    pub env: EnvPolicy,
    #[serde(skip)]
    pub stdin: StdinSource,
    pub timeout: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    None,
    All,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EnvPolicy {
    #[default]
    Baseline,
    Allowlist {
        vars: Vec<String>,
    },
}

#[derive(Debug, Clone, Default)]
pub enum StdinSource {
    #[default]
    Null,
    Inherit,
    Bytes(Vec<u8>),
}

#[derive(Debug)]
pub struct SandboxOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub elapsed: Duration,
    pub timed_out: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Bwrap,
    SandboxExec,
    Docker,
}
