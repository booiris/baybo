use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

pub const PROCESS_TOKEN_ENV: &str = "BAYBO_PROCESS_TOKEN";

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub struct ProcessManagerConfig {
    pub ledger_dir: Option<PathBuf>,
}

#[derive(Debug)]
pub struct ProcessManager {
    ledger_dir: Option<PathBuf>,
    tracked: Mutex<BTreeMap<i32, TrackedProcess>>,
}

#[derive(Debug)]
struct TrackedProcess {
    label: String,
    token: String,
}

#[derive(Serialize, Deserialize)]
struct PersistedProcess {
    pgid: i32,
    label: String,
    token: String,
}

impl ProcessManager {
    pub fn from_config(config: ProcessManagerConfig) -> Arc<Self> {
        if let Some(dir) = config.ledger_dir.as_deref() {
            let reaped = reap_stale_ledger(dir);
            if reaped > 0 {
                tracing::warn!(reaped, path = %dir.display(), "reaped process groups from a prior runtime");
            }
        }
        Arc::new(Self {
            ledger_dir: config.ledger_dir,
            tracked: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn transient() -> Arc<Self> {
        Self::from_config(ProcessManagerConfig { ledger_dir: None })
    }

    pub fn spawn(
        self: &Arc<Self>,
        command: &mut Command,
        label: impl Into<String>,
    ) -> io::Result<ManagedChild> {
        let label = label.into();
        let token = next_token();
        command
            .env(PROCESS_TOKEN_ENV, &token)
            .process_group(0)
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let Some(pid) = child.id() else {
            let _ = child.start_kill();
            return Err(io::Error::other("spawned child has no process id"));
        };
        let pgid = pid as i32;
        self.register(pgid, &label, &token);
        Ok(ManagedChild {
            child: Some(child),
            registration: Registration {
                manager: Arc::clone(self),
                pgid,
                armed: true,
            },
        })
    }

    pub fn tracked_len(&self) -> usize {
        self.tracked.lock().len()
    }

    pub fn kill_all_now(&self) {
        for pgid in self.snapshot_groups() {
            signal_group(pgid, libc::SIGKILL);
        }
    }

    pub async fn shutdown_all(&self, grace: Duration) {
        let groups = self.snapshot_groups();
        for pgid in &groups {
            signal_group(*pgid, libc::SIGTERM);
        }
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            let alive: Vec<i32> = groups
                .iter()
                .copied()
                .filter(|pgid| group_alive(*pgid))
                .collect();
            if alive.is_empty() {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                for pgid in alive {
                    signal_group(pgid, libc::SIGKILL);
                }
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn register(&self, pgid: i32, label: &str, token: &str) {
        self.tracked.lock().insert(
            pgid,
            TrackedProcess {
                label: label.to_string(),
                token: token.to_string(),
            },
        );
        let Some(dir) = self.ledger_dir.as_deref() else {
            return;
        };
        let record = PersistedProcess {
            pgid,
            label: label.to_string(),
            token: token.to_string(),
        };
        if let Ok(json) = serde_json::to_vec(&record)
            && std::fs::create_dir_all(dir).is_ok()
        {
            let _ = std::fs::write(ledger_path(dir, token), json);
        }
    }

    fn unregister(&self, pgid: i32) {
        let removed = self.tracked.lock().remove(&pgid);
        if let (Some(dir), Some(record)) = (self.ledger_dir.as_deref(), removed) {
            let _ = std::fs::remove_file(ledger_path(dir, &record.token));
            tracing::debug!(pgid, label = %record.label, "process group released");
        }
    }

    fn snapshot_groups(&self) -> Vec<i32> {
        self.tracked.lock().keys().copied().collect()
    }
}

pub struct ManagedChild {
    child: Option<Child>,
    registration: Registration,
}

impl ManagedChild {
    pub fn id(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }

    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.as_mut()?.stdin.take()
    }

    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.as_mut()?.stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.as_mut()?.stderr.take()
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = match self.child.as_mut() {
            Some(child) => child.try_wait()?,
            None => return Err(io::Error::other("child already consumed")),
        };
        if status.is_some() {
            self.finish();
        }
        Ok(status)
    }

    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        let result = match self.child.as_mut() {
            Some(child) => child.wait().await,
            None => Err(io::Error::other("child already consumed")),
        };
        self.finish();
        result
    }

    pub async fn wait_with_output(mut self) -> io::Result<Output> {
        let Some(child) = self.child.take() else {
            return Err(io::Error::other("child already consumed"));
        };
        let result = child.wait_with_output().await;
        self.finish();
        result
    }

    pub fn start_kill(&mut self) -> io::Result<()> {
        signal_group(self.registration.pgid, libc::SIGKILL);
        match self.child.as_mut() {
            Some(child) => child.start_kill(),
            None => Ok(()),
        }
    }

    pub async fn shutdown(&mut self, grace: Duration) -> io::Result<ExitStatus> {
        signal_group(self.registration.pgid, libc::SIGTERM);
        let wait = async {
            match self.child.as_mut() {
                Some(child) => child.wait().await,
                None => Err(io::Error::other("child already consumed")),
            }
        };
        let result = match tokio::time::timeout(grace, wait).await {
            Ok(result) => result,
            Err(_) => {
                signal_group(self.registration.pgid, libc::SIGKILL);
                match self.child.as_mut() {
                    Some(child) => child.wait().await,
                    None => Err(io::Error::other("child already consumed")),
                }
            }
        };
        self.finish();
        result
    }

    fn finish(&mut self) {
        if self.registration.armed {
            signal_group(self.registration.pgid, libc::SIGKILL);
            self.registration.disarm();
        }
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if self.registration.armed {
            signal_group(self.registration.pgid, libc::SIGKILL);
            if let Some(child) = self.child.as_mut() {
                let _ = child.start_kill();
            }
            self.registration.disarm();
        }
    }
}

struct Registration {
    manager: Arc<ProcessManager>,
    pgid: i32,
    armed: bool,
}

impl Registration {
    fn disarm(&mut self) {
        if self.armed {
            self.manager.unregister(self.pgid);
            self.armed = false;
        }
    }
}

fn next_token() -> String {
    let sequence = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("bp-{}-{sequence}-{nanos}", std::process::id())
}

fn ledger_path(dir: &Path, token: &str) -> PathBuf {
    dir.join(format!("{token}.json"))
}

fn signal_group(pgid: i32, signal: i32) {
    unsafe {
        libc::kill(-pgid, signal);
    }
}

fn group_alive(pgid: i32) -> bool {
    let result = unsafe { libc::kill(-pgid, 0) };
    result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub fn reap_stale_ledger(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut reaped = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        if let Ok(bytes) = std::fs::read(&path)
            && let Ok(record) = serde_json::from_slice::<PersistedProcess>(&bytes)
            && group_has_token(record.pgid, &record.token)
        {
            signal_group(record.pgid, libc::SIGKILL);
            reaped += 1;
        }
        let _ = std::fs::remove_file(path);
    }
    reaped
}

#[cfg(target_os = "linux")]
fn group_has_token(pgid: i32, token: &str) -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    let needle = format!("{PROCESS_TOKEN_ENV}={token}");
    entries.flatten().any(|entry| {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            return false;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        if proc_stat_field(&stat, 5).and_then(|value| value.parse::<i32>().ok()) != Some(pgid) {
            return false;
        }
        std::fs::read(format!("/proc/{pid}/environ")).is_ok_and(|env| {
            env.split(|byte| *byte == 0)
                .any(|item| item == needle.as_bytes())
        })
    })
}

#[cfg(target_os = "linux")]
fn proc_stat_field(stat: &str, field: usize) -> Option<&str> {
    let after_comm = stat.get(stat.rfind(')')? + 1..)?.trim_start();
    after_comm.split_whitespace().nth(field.checked_sub(3)?)
}

#[cfg(target_os = "macos")]
fn group_has_token(pgid: i32, token: &str) -> bool {
    const PROC_ALL_PIDS: u32 = 1;
    let bytes = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
    if bytes <= 0 {
        return false;
    }
    let mut pids = vec![0_i32; bytes as usize / std::mem::size_of::<i32>() + 32];
    let capacity = (pids.len() * std::mem::size_of::<i32>()) as i32;
    let read = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, pids.as_mut_ptr().cast(), capacity) };
    if read <= 0 {
        return false;
    }
    let count = read as usize / std::mem::size_of::<i32>();
    let needle = format!("{PROCESS_TOKEN_ENV}={token}");
    pids.into_iter().take(count).any(|pid| {
        pid > 0
            && unsafe { libc::getpgid(pid) } == pgid
            && macos_process_env(pid).iter().any(|item| item == &needle)
    })
}

#[cfg(target_os = "macos")]
fn macos_process_env(pid: i32) -> Vec<String> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size = 0_usize;
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || size < std::mem::size_of::<i32>()
    {
        return Vec::new();
    }
    let mut buffer = vec![0_u8; size];
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Vec::new();
    }
    buffer.truncate(size);
    let argc = i32::from_ne_bytes(buffer[..4].try_into().unwrap_or_default()).max(0) as usize;
    let mut fields = buffer[4..]
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    let _ = fields.next();
    for _ in 0..argc {
        let _ = fields.next();
    }
    fields
        .filter_map(|field| std::str::from_utf8(field).ok().map(str::to_string))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn wait_for_pid_file(path: &Path) -> i32 {
        for _ in 0..100 {
            if let Ok(raw) = tokio::fs::read_to_string(path).await
                && let Ok(pid) = raw.trim().parse()
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("pid file was not written: {}", path.display());
    }

    async fn wait_until_gone(pid: i32) -> bool {
        for _ in 0..100 {
            if unsafe { libc::kill(pid, 0) } != 0 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    #[tokio::test]
    async fn dropping_child_kills_its_process_tree() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pid_file = temp.path().join("grandchild.pid");
        let manager = ProcessManager::transient();
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(format!("sleep 30 & echo $! > {}; wait", pid_file.display()));
        let child = manager.spawn(&mut command, "drop-tree").expect("spawn");
        let grandchild = wait_for_pid_file(&pid_file).await;
        drop(child);
        if !wait_until_gone(grandchild).await {
            unsafe { libc::kill(grandchild, libc::SIGKILL) };
            panic!("grandchild {grandchild} survived managed-child drop");
        }
        assert_eq!(manager.tracked_len(), 0);
    }

    #[tokio::test]
    async fn clean_wait_kills_unclaimed_descendants() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pid_file = temp.path().join("grandchild.pid");
        let manager = ProcessManager::transient();
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(format!("sleep 30 & echo $! > {}", pid_file.display()));
        let mut child = manager.spawn(&mut command, "wait-tree").expect("spawn");
        let grandchild = wait_for_pid_file(&pid_file).await;
        child.wait().await.expect("wait");
        if !wait_until_gone(grandchild).await {
            unsafe { libc::kill(grandchild, libc::SIGKILL) };
            panic!("grandchild {grandchild} survived managed-child wait");
        }
        assert_eq!(manager.tracked_len(), 0);
    }

    #[tokio::test]
    async fn next_manager_reaps_crash_ledger() {
        let temp = tempfile::tempdir().expect("tempdir");
        let ledger = temp.path().join("ledger");
        let pid_file = temp.path().join("grandchild.pid");
        let manager = ProcessManager::from_config(ProcessManagerConfig {
            ledger_dir: Some(ledger.clone()),
        });
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(format!("sleep 30 & echo $! > {}; wait", pid_file.display()));
        let child = manager.spawn(&mut command, "ledger-tree").expect("spawn");
        let grandchild = wait_for_pid_file(&pid_file).await;
        std::mem::forget(child);

        let _next = ProcessManager::from_config(ProcessManagerConfig {
            ledger_dir: Some(ledger),
        });
        if !wait_until_gone(grandchild).await {
            manager.kill_all_now();
            panic!("grandchild {grandchild} survived crash-ledger reap");
        }
    }
}
