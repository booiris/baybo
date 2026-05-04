//! Filesystem path sensitivity checks.
//!
//! Blocks access to credential-bearing files (SSH/AWS/GCP/Azure configs,
//! `.env` variants, PEM/JKS key material, shell history, etc.). The
//! check is path-string based; `is_sensitive_path` canonicalizes
//! internally and falls back to the literal path if canonicalization
//! fails (e.g. when the file does not exist yet).

use std::path::Path;

const SENSITIVE_PATH_PATTERNS: &[&str] = &[
    "/.ssh/",
    "/.aws/",
    "/.azure/",
    "/.gcloud/",
    "/.config/gcloud/",
    "/.config/gh/hosts.yml",
    "/.docker/",
    "/.kube/",
    "/.gnupg/",
    "/.netrc",
    "/.pgpass",
    "/.npmrc",
    "/.pypirc",
    "/.git-credentials",
    "/.vault-token",
    "/.terraform.d/credentials.tfrc.json",
    "/etc/shadow",
    "/etc/gshadow",
    "/etc/passwd",
    "/etc/sudoers",
    "/.bash_history",
    "/.zsh_history",
    "/.histfile",
    "/.python_history",
    "/.node_repl_history",
    "/.mysql_history",
    "/.psql_history",
];

const SENSITIVE_EXTENSIONS: &[&str] = &[".pem", ".key", ".p12", ".pfx", ".jks", ".keystore"];

const SAFE_EXT_SUFFIXES: &[&str] = &[".dist"];

const ENV_SAFE_SUFFIXES: &[&str] = &[".example", ".template", ".sample", ".dist"];

const SENSITIVE_FILENAMES: &[&str] = &[
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
    "id_dsa",
    "authorized_keys",
    "known_hosts",
];

/// Returns true if `path` points at a credential-bearing file or directory.
///
/// Matching is done on the lowercased, forward-slash-normalized path
/// string. The function canonicalizes internally via
/// `std::fs::canonicalize`; if canonicalization fails (e.g. the path
/// does not yet exist), it falls back to the literal path.
pub fn is_sensitive_path(path: &Path) -> bool {
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let path_str = resolved
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();

    if let Some(filename) = resolved.file_name().and_then(|f| f.to_str()) {
        let filename_lower = filename.to_ascii_lowercase();

        // `.env`, `.env.local`, `.env.production` → sensitive.
        // `.env.example`, `.env.template`, `.env.sample`, `.env.dist` → allowed.
        if filename_lower == ".env" || filename_lower.starts_with(".env.") {
            let remainder = filename_lower.strip_prefix(".env").unwrap_or("");
            if !ENV_SAFE_SUFFIXES.contains(&remainder) {
                return true;
            }
        }

        let has_safe_ext_suffix = SAFE_EXT_SUFFIXES
            .iter()
            .any(|s| filename_lower.ends_with(s));
        if !has_safe_ext_suffix
            && SENSITIVE_EXTENSIONS
                .iter()
                .any(|ext| filename_lower.ends_with(ext))
        {
            return true;
        }

        if SENSITIVE_FILENAMES.iter().any(|&f| filename_lower == f) {
            return true;
        }
    }

    let path_str_with_slash = format!("{}/", path_str);
    SENSITIVE_PATH_PATTERNS
        .iter()
        .any(|p| path_str.contains(*p) || path_str_with_slash.contains(*p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_ssh_dir() {
        assert!(is_sensitive_path(Path::new("/home/u/.ssh/id_rsa")));
        assert!(is_sensitive_path(Path::new("/home/u/.ssh/authorized_keys")));
        assert!(is_sensitive_path(Path::new("/home/u/.ssh/config")));
    }

    #[test]
    fn blocks_aws_dir() {
        assert!(is_sensitive_path(Path::new("/home/u/.aws/credentials")));
        assert!(is_sensitive_path(Path::new(
            "/home/u/.aws/cli/cache/token.json"
        )));
    }

    #[test]
    fn blocks_dotenv() {
        assert!(is_sensitive_path(Path::new("/app/.env")));
        assert!(is_sensitive_path(Path::new("/app/.env.local")));
        assert!(is_sensitive_path(Path::new("/app/.env.production")));
    }

    #[test]
    fn allows_env_examples() {
        assert!(!is_sensitive_path(Path::new("/app/.env.example")));
        assert!(!is_sensitive_path(Path::new("/app/.env.sample")));
        assert!(!is_sensitive_path(Path::new("/app/.env.template")));
    }

    #[test]
    fn blocks_key_material() {
        assert!(is_sensitive_path(Path::new("/opt/certs/server.key")));
        assert!(is_sensitive_path(Path::new("/opt/certs/server.pem")));
        assert!(is_sensitive_path(Path::new("/opt/certs/cert.p12")));
    }

    #[test]
    fn allows_safe_ext_suffix() {
        assert!(!is_sensitive_path(Path::new("/opt/certs/server.key.dist")));
    }

    #[test]
    fn blocks_etc_shadow() {
        assert!(is_sensitive_path(Path::new("/etc/shadow")));
        assert!(is_sensitive_path(Path::new("/etc/gshadow")));
        assert!(is_sensitive_path(Path::new("/etc/sudoers")));
    }

    #[test]
    fn blocks_shell_history() {
        assert!(is_sensitive_path(Path::new("/home/u/.bash_history")));
        assert!(is_sensitive_path(Path::new("/home/u/.zsh_history")));
    }

    #[test]
    fn ignores_ordinary_files() {
        assert!(!is_sensitive_path(Path::new("/home/u/project/src/main.rs")));
        assert!(!is_sensitive_path(Path::new("/tmp/notes.md")));
    }

    #[test]
    fn file_named_id_rsa_but_not_under_ssh_is_still_blocked() {
        assert!(is_sensitive_path(Path::new("/tmp/id_rsa")));
    }

    #[test]
    fn substring_within_filename_does_not_block() {
        // "grid_rsa_data" is NOT exactly "id_rsa"; filename match is exact.
        assert!(!is_sensitive_path(Path::new("/tmp/grid_rsa_data")));
    }
}
