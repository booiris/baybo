use std::path::PathBuf;

use crate::error::SandboxError;
use crate::spec::Backend;

#[derive(Debug, Clone)]
pub struct SandboxAvailability {
    pub backend: Backend,
    pub binary_path: PathBuf,
    pub version: Option<String>,
}

pub(crate) fn locate_binary(name: &'static str) -> Result<PathBuf, SandboxError> {
    let path_var = std::env::var_os("PATH").ok_or(SandboxError::BackendMissing { name })?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                match std::fs::metadata(&candidate) {
                    Ok(m) if m.permissions().mode() & 0o111 != 0 => return Ok(candidate),
                    Ok(_) => {
                        return Err(SandboxError::BackendNotExecutable {
                            name,
                            path: candidate,
                        });
                    }
                    Err(_) => continue,
                }
            }
            #[cfg(not(unix))]
            {
                return Ok(candidate);
            }
        }
    }
    Err(SandboxError::BackendMissing { name })
}

pub(crate) fn parse_version(stdout: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(stdout).ok()?;
    let line = s.lines().next()?;
    Some(line.trim().to_string())
}
