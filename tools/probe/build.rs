//! Records the handy-recorder commit the probe was built from, for the
//! results file.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn main() {
    let commit = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|s| !s.is_empty());
    println!(
        "cargo:rustc-env=PROBE_GIT_COMMIT={commit}{}",
        if dirty { " (with uncommitted changes)" } else { "" }
    );
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
}
