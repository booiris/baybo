//! Filesystem-grounded tests for `is_sensitive_path`.
//!
//! Unlike the in-module unit tests, these exercise the canonicalization
//! step against real on-disk files, including symlink scenarios that
//! string-only checks miss.

use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::PathBuf;

use baybo_security::is_sensitive_path;
use tempfile::TempDir;

fn write(path: &std::path::Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

#[test]
fn dotenv_under_real_dir_is_blocked() {
    let dir = TempDir::new().unwrap();
    let env = dir.path().join(".env");
    write(&env, "SECRET=1");
    assert!(is_sensitive_path(&env));
}

#[test]
fn dotenv_example_is_allowed() {
    let dir = TempDir::new().unwrap();
    let example = dir.path().join(".env.example");
    write(&example, "SECRET=changeme");
    assert!(!is_sensitive_path(&example));
}

#[test]
fn pem_under_temp_dir_is_blocked() {
    let dir = TempDir::new().unwrap();
    let pem = dir.path().join("server.pem");
    write(&pem, "-----BEGIN PRIVATE KEY-----\n...\n");
    assert!(is_sensitive_path(&pem));
}

#[test]
fn symlink_to_dotenv_is_blocked_after_canonicalization() {
    let dir = TempDir::new().unwrap();
    let real = dir.path().join(".env");
    write(&real, "SECRET=1");
    let link = dir.path().join("innocent.txt");
    unix_fs::symlink(&real, &link).unwrap();

    // String-only check would see "innocent.txt"; canonicalization in
    // `is_sensitive_path` must collapse the link to the real `.env`.
    assert!(is_sensitive_path(&link));
}

#[test]
fn symlink_to_safe_file_stays_safe() {
    let dir = TempDir::new().unwrap();
    let real = dir.path().join("notes.md");
    write(&real, "hello");
    let link = dir.path().join(".env");
    unix_fs::symlink(&real, &link).unwrap();

    // The link's path string contains `.env` but it canonicalizes to
    // `notes.md` — must be allowed. Confirms canonicalization wins
    // over surface filename matching.
    assert!(!is_sensitive_path(&link));
}

#[test]
fn ssh_dirlike_path_is_blocked_even_without_real_file() {
    // Pure-string match fallback: path doesn't exist on disk, so
    // canonicalize() fails and the function falls back to the literal.
    let p = PathBuf::from("/nonexistent/home/user/.ssh/id_ed25519");
    assert!(is_sensitive_path(&p));
}

#[test]
fn ordinary_source_file_under_temp_is_safe() {
    let dir = TempDir::new().unwrap();
    let src = dir.path().join("src").join("main.rs");
    write(&src, "fn main() {}");
    assert!(!is_sensitive_path(&src));
}

#[test]
fn key_dist_suffix_is_allowed() {
    let dir = TempDir::new().unwrap();
    let f = dir.path().join("server.key.dist");
    write(&f, "template");
    assert!(!is_sensitive_path(&f));
}
