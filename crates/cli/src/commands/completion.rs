use std::io::Cursor;

use clap::CommandFactory;
use clap_complete::{Shell, generate};

use crate::cli::{Cli, ShellKind};
use crate::error::Result;
use crate::format::CommandOutput;

pub fn handle(shell: ShellKind) -> Result<CommandOutput> {
    let target = match shell {
        ShellKind::Bash => Shell::Bash,
        ShellKind::Zsh => Shell::Zsh,
        ShellKind::Fish => Shell::Fish,
        ShellKind::Powershell => Shell::PowerShell,
        ShellKind::Elvish => Shell::Elvish,
    };
    let mut cmd = Cli::command();
    let mut buf: Vec<u8> = Vec::new();
    {
        let mut sink = Cursor::new(&mut buf);
        generate(target, &mut cmd, "aura", &mut sink);
    }
    let script =
        String::from_utf8(buf).map_err(|e| crate::error::CliError::Serialization(e.to_string()))?;
    Ok(CommandOutput {
        human: script,
        data: None,
    })
}
