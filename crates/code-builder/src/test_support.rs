use std::sync::Arc;

use async_trait::async_trait;
use aura_sandbox::{
    Backend, ResourceLimits, SandboxError, SandboxOutput, SandboxRunner, SandboxSpec,
};
use parking_lot::Mutex;

/// Fake `SandboxRunner` for unit tests. Captures every `SandboxSpec` it
/// receives and returns either a fixed `SandboxOutput` or a fixed
/// `SandboxError`.
pub(crate) struct FakeSandboxRunner {
    captured: Mutex<Vec<SandboxSpec>>,
    outcome: Mutex<Outcome>,
}

enum Outcome {
    Ok(SandboxOutput),
    Err(SandboxError),
    Consumed,
}

impl FakeSandboxRunner {
    pub fn new(out: SandboxOutput) -> Arc<Self> {
        Arc::new(Self {
            captured: Mutex::new(Vec::new()),
            outcome: Mutex::new(Outcome::Ok(out)),
        })
    }

    pub fn with_error(err: SandboxError) -> Arc<Self> {
        Arc::new(Self {
            captured: Mutex::new(Vec::new()),
            outcome: Mutex::new(Outcome::Err(err)),
        })
    }

    pub fn captured(&self) -> Vec<SandboxSpec> {
        self.captured.lock().clone()
    }
}

#[async_trait]
impl SandboxRunner for FakeSandboxRunner {
    async fn run(&self, spec: SandboxSpec) -> Result<SandboxOutput, SandboxError> {
        self.captured.lock().push(spec);
        let mut slot = self.outcome.lock();
        let outcome = std::mem::replace(&mut *slot, Outcome::Consumed);
        match outcome {
            Outcome::Ok(out) => {
                *slot = Outcome::Ok(SandboxOutput {
                    exit_code: out.exit_code,
                    stdout: out.stdout.clone(),
                    stderr: out.stderr.clone(),
                    elapsed: out.elapsed,
                    timed_out: out.timed_out,
                });
                Ok(out)
            }
            Outcome::Err(e) => {
                let msg = e.to_string();
                *slot = Outcome::Err(SandboxError::BackendFailure {
                    status: None,
                    stderr: msg,
                });
                Err(e)
            }
            Outcome::Consumed => Err(SandboxError::BackendFailure {
                status: None,
                stderr: "outcome already consumed".into(),
            }),
        }
    }

    fn backend(&self) -> Backend {
        Backend::Docker
    }

    fn default_resource_limits(&self) -> ResourceLimits {
        ResourceLimits::safe_defaults()
    }
}
