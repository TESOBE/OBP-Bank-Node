// Copyright (C) 2025-2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later
// Licensed under the GNU Affero General Public License v3 or later: see the LICENSE and NOTICE files.

//! Stamp the build with its git commit, branch, dirtiness and time, for the root `/status` page, as the node's `build.rs` and OBP-API's `git.properties` do. Each falls back to "unknown" when git is unavailable, for example when building from a source tarball.

// Run git and date as child processes.
use std::process::Command;

// Run git with these arguments and return what it printed, or None when git is missing or fails.
fn git(args: &[&str]) -> Option<String> {
    // Run git and wait for it; a git that cannot be started gives None.
    let output = Command::new("git").args(args).output().ok()?;
    // Keep the trimmed output only when git succeeded.
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

// Cargo runs this before compiling the crate.
fn main() {
    // The commit the build was made from.
    let commit = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into());
    // The branch checked out at the time.
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|| "unknown".into());
    // Whether tracked files had uncommitted changes: any output from git status means they did.
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).map(|porcelain| (!porcelain.is_empty()).to_string()).unwrap_or_else(|| "false".into());
    // Ask date for the build time in ISO 8601 with the UTC offset.
    let date_output = Command::new("date").arg("+%Y-%m-%dT%H:%M:%S%:z").output().ok();
    // Keep the time only when date succeeded, otherwise say "unknown".
    let build_time = date_output.filter(|output| output.status.success()).map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string()).unwrap_or_else(|| "unknown".into());

    // Hand the commit to the compiler, read back with env!().
    println!("cargo:rustc-env=OBP_BANK_NODE_UI_GIT_COMMIT={commit}");
    // Hand the branch to the compiler.
    println!("cargo:rustc-env=OBP_BANK_NODE_UI_GIT_BRANCH={branch}");
    // Hand the dirtiness to the compiler.
    println!("cargo:rustc-env=OBP_BANK_NODE_UI_GIT_DIRTY={dirty}");
    // Hand the build time to the compiler.
    println!("cargo:rustc-env=OBP_BANK_NODE_UI_BUILD_TIME={build_time}");
    // Stamp again when the checkout moves to another commit or branch.
    for path in ["../../.git/HEAD", "../../.git/index"] {
        // Tell cargo to rerun this script when that file changes.
        println!("cargo:rerun-if-changed={path}");
    }
}
