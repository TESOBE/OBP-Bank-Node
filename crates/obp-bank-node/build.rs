//! Stamp the build with its git commit, branch, dirtiness and time, for the
//! root `/status` page (as OBP-API's `git.properties` does). Each falls back to
//! "unknown" when git is unavailable, e.g. building from a source tarball.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    let commit = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
        .map(|s| (!s.is_empty()).to_string())
        .unwrap_or_else(|| "false".into());
    let time = Command::new("date")
        .arg("+%Y-%m-%dT%H:%M:%S%:z")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into());

    println!("cargo:rustc-env=OBP_BANK_NODE_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=OBP_BANK_NODE_GIT_BRANCH={branch}");
    println!("cargo:rustc-env=OBP_BANK_NODE_GIT_DIRTY={dirty}");
    println!("cargo:rustc-env=OBP_BANK_NODE_BUILD_TIME={time}");
    // Re-stamp when the checkout moves to another commit or branch.
    for path in ["../../.git/HEAD", "../../.git/index"] {
        println!("cargo:rerun-if-changed={path}");
    }
}
