use std::path::Path;

use crate::ToolError;

pub(super) fn require_absolute(path: &Path, tool: &str, field: &str) -> crate::Result<()> {
    if !path.is_absolute() {
        return Err(ToolError::InvalidParams(format!(
            "{tool} `{field}` must be an absolute path, got `{}`",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn require_absolute_accepts_absolute() {
        assert!(require_absolute(&PathBuf::from("/tmp/x"), "T", "p").is_ok());
    }

    #[test]
    fn require_absolute_rejects_relative() {
        let err = require_absolute(&PathBuf::from("rel/x"), "T", "p").unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(ref m) if m.contains("absolute")));
    }
}
