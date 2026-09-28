//! Stamps the binary with the commit it was built from, so every eval result
//! names the harness revision that produced it.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn main() {
    if let Some(dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
        println!("cargo:rerun-if-changed={dir}/index");
    }
    // Directories are scanned recursively, so a source edit refreshes `-dirty`.
    println!("cargo:rerun-if-changed=../../crates");
    println!("cargo:rerun-if-changed=src");
    let commit = git(&["describe", "--always", "--dirty", "--exclude", "*"])
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=HOLT_RUNNER_COMMIT={commit}");
}
