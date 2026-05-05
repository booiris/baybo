use std::io::{self, BufRead, IsTerminal, Write};

use crate::error::{CliError, Result};

/// Write `label`, read one line from `reader`, return the trimmed
/// string. Closed stdin is a hard error so the caller can't loop on
/// an empty buffer forever.
pub(crate) fn prompt_line<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    label: &str,
) -> Result<String> {
    writer.write_all(label.as_bytes())?;
    writer.flush()?;
    let mut buf = String::new();
    let bytes = reader
        .read_line(&mut buf)
        .map_err(|e| CliError::Config(format!("failed to read interactive input: {e}")))?;
    if bytes == 0 {
        return Err(CliError::Io(
            "stdin closed while reading interactive input".into(),
        ));
    }
    Ok(buf.trim().to_string())
}

/// `[y/N]` confirmation prompt over stdin/stderr. Returns `true`
/// only on an explicit yes; everything else (including empty input)
/// is `false`.
pub(crate) fn confirm(question: &str) -> Result<bool> {
    let stdin = io::stdin();
    let stderr = io::stderr();
    if !stdin.is_terminal() || !stderr.is_terminal() {
        return Err(CliError::Config(
            "interactive confirmation requires a terminal".into(),
        ));
    }
    let mut reader = stdin.lock();
    let mut writer = stderr.lock();
    let label = format!("{question} [y/N]: ");
    let ans = prompt_line(&mut reader, &mut writer, &label)?;
    Ok(matches!(
        ans.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Like `prompt_line` but substitutes `default` when the user
/// presses enter on an empty line. The default is shown inline as
/// `label [default]: `; pass `""` for default-less prompts.
pub(crate) fn prompt_with_default(label: &str, default: &str) -> Result<String> {
    let stdin = io::stdin();
    let stderr = io::stderr();
    let mut reader = stdin.lock();
    let mut writer = stderr.lock();
    let display = if default.is_empty() {
        format!("{label}: ")
    } else {
        format!("{label} [{default}]: ")
    };
    let value = prompt_line(&mut reader, &mut writer, &display)?;
    if value.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(value)
    }
}
