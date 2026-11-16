// Copyright (C) 2025-2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later
// Licensed under the GNU Affero General Public License v3 or later: see the LICENSE and NOTICE files.

//! The OBP-API version every Bank-Node call uses.
//!
//! OBP-API deployments can switch off the versions below v5.1.0, so every
//! path the Bank-Node, its UI and its dev scripts send to OBP-API must use
//! v5.1.0 or newer. This test reads the sources of the whole workspace and
//! fails on any `obp/vX.Y.Z` below v5.1.0 that is not listed in
//! [`BELOW_V510`], so a new call on an old version cannot slip in. It also
//! reads the `OBP_API_VERSION` constant the Bank-Node builds its URLs from, and
//! the paths in the contract files, which name the version the calls must use.

use std::fs;
use std::path::{Path, PathBuf};

/// `file|fragment` -> why that path is still below v5.1.0. A line is allowed
/// when it is in `file` (relative to the workspace root) and contains
/// `fragment`. Empty: every call has a v5.1.0 or newer equivalent.
const BELOW_V510: &[(&str, &str)] = &[];

/// Where OBP-API paths are written: Rust code, the UI's static pages, and the
/// dev scripts and configs.
const SOURCE_ROOTS: &[&str] = &["crates", "dev", "docker", "obp-api-contract"];
const EXTENSIONS: &[&str] = &["rs", "sh", "js", "html", "yaml", "yml", "toml", "json"];
const SKIP_DIRS: &[&str] = &["target", "node_modules"];
/// This file, whose examples and list name old versions on purpose.
const THIS_FILE: &str = "crates/obp-bank-node-ui/tests/obp_api_version.rs";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn source_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        if path.is_dir() {
            source_files(&path, out);
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| EXTENSIONS.contains(&e))
        {
            out.push(path);
        }
    }
}

/// Every `obp/vX.Y.Z` in `line` whose version is below v5.1.0, as written.
fn versions_below_v510(line: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = line;
    while let Some(at) = rest.find("obp/v") {
        let after = &rest[at + "obp/v".len()..];
        let version: String = after.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
        let parts: Vec<u32> = version.split('.').filter_map(|p| p.parse().ok()).collect();
        if parts.len() == 3 && version.split('.').count() == 3 && (parts[0], parts[1], parts[2]) < (5, 1, 0) {
            found.push(format!("obp/v{version}"));
        }
        rest = after;
    }
    found
}

/// Every `OBP_API_VERSION ... "vX.Y.Z"` in `line` whose version is below v5.1.0: the constant the
/// Bank-Node puts into the URLs it builds, which carry no literal `obp/vX.Y.Z`.
fn version_constants_below_v510(line: &str) -> Vec<String> {
    let Some(at) = line.find("OBP_API_VERSION") else { return vec![] };
    let after = &line[at + "OBP_API_VERSION".len()..];
    let Some(quote) = after.find("\"v") else { return vec![] };
    let version: String = after[quote + 2..].chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let parts: Vec<u32> = version.split('.').filter_map(|p| p.parse().ok()).collect();
    if parts.len() == 3 && version.split('.').count() == 3 && (parts[0], parts[1], parts[2]) < (5, 1, 0) {
        vec![format!("OBP_API_VERSION v{version}")]
    } else {
        vec![]
    }
}

fn calls_below_v510() -> Vec<String> {
    let root = workspace_root();
    let mut files = Vec::new();
    for dir in SOURCE_ROOTS {
        source_files(&root.join(dir), &mut files);
    }
    let mut found = Vec::new();
    for file in files {
        let rel = file.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
        if rel == THIS_FILE {
            continue;
        }
        let Ok(text) = fs::read_to_string(&file) else { continue };
        for (i, line) in text.lines().enumerate() {
            for version in versions_below_v510(line).into_iter().chain(version_constants_below_v510(line)) {
                let allowed = BELOW_V510.iter().any(|(key, _)| {
                    let (key_file, fragment) = key.split_once('|').unwrap();
                    key_file == rel && line.contains(fragment)
                });
                if !allowed {
                    found.push(format!("{rel}:{} {version}", i + 1));
                }
            }
        }
    }
    found
}

#[test]
fn finds_the_workspace_sources() {
    // Tests that the scan reads the real sources, so an empty result means "no old version", not "no files".
    let mut files = Vec::new();
    source_files(&workspace_root().join("crates"), &mut files);
    assert!(files.len() > 20, "found only {} source files", files.len());
}

#[test]
fn calls_obp_api_on_v510_or_newer() {
    // Tests that no OBP-API path below v5.1.0 exists outside the listed call sites.
    assert_eq!(calls_below_v510(), Vec::<String>::new());
}

#[test]
fn every_listed_call_site_still_exists() {
    // Tests that each entry in BELOW_V510 still matches a line, so a call that moved to v5.1.0 is also removed from the list.
    let root = workspace_root();
    let stale: Vec<&str> = BELOW_V510
        .iter()
        .map(|(key, _)| *key)
        .filter(|key| {
            let (file, fragment) = key.split_once('|').unwrap();
            !fs::read_to_string(root.join(file)).is_ok_and(|text| text.contains(fragment))
        })
        .collect();
    assert_eq!(stale, Vec::<&str>::new());
}

#[test]
fn recognises_the_ways_a_path_below_v510_can_be_written() {
    // Tests the matcher itself: old versions are found with or without a leading slash, newer ones are not.
    assert_eq!(versions_below_v510(r#"format!("/obp/v2.2.0/banks/{b}/fx")"#), vec!["obp/v2.2.0"]);
    assert_eq!(versions_below_v510("api POST obp/v5.0.0/banks"), vec!["obp/v5.0.0"]);
    assert_eq!(versions_below_v510(r#""{}/obp/v4.0.0/x" and "/obp/v1.2.1/y""#), vec!["obp/v4.0.0", "obp/v1.2.1"]);
    assert!(versions_below_v510("/obp/v5.1.0/banks").is_empty());
    assert!(versions_below_v510("/obp/v6.0.0/root and /obp/v7.0.0/routing-schemes").is_empty());
    assert!(versions_below_v510("/obp/dynamic-entity/banks/x").is_empty());
}

#[test]
fn recognises_an_old_version_in_the_version_constant() {
    // Tests the matcher itself: the constant the URLs are built from is read like a literal path.
    assert_eq!(
        version_constants_below_v510(r#"const OBP_API_VERSION: &str = "v4.0.0";"#),
        vec!["OBP_API_VERSION v4.0.0"]
    );
    assert!(version_constants_below_v510(r#"const OBP_API_VERSION: &str = "v7.0.0";"#).is_empty());
    assert!(version_constants_below_v510(r#"const OBP_API_VERSION: &str = "v5.1.0";"#).is_empty());
    assert!(version_constants_below_v510("ver = OBP_API_VERSION,").is_empty());
}

#[test]
fn reads_the_version_constant_and_the_contract() {
    // Tests that the scan reaches the two places a literal `obp/vX.Y.Z` hides or is recorded: the constant
    // in obp_client.rs, and the paths in the contract files.
    let root = workspace_root();
    let client = fs::read_to_string(root.join("crates/obp-bank-node/src/obp_client.rs")).unwrap();
    assert!(client.lines().any(|l| l.contains("OBP_API_VERSION: &str = \"v")));
    let mut files = Vec::new();
    for dir in SOURCE_ROOTS {
        source_files(&root.join(dir), &mut files);
    }
    let names: Vec<String> = files.iter().map(|f| f.file_name().unwrap().to_string_lossy().to_string()).collect();
    assert!(names.contains(&"contract.json".to_string()) && names.contains(&"snapshot.json".to_string()));
}
