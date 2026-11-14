// Copyright (C) 2025-2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later
// Licensed under the GNU Affero General Public License v3 or later: see the LICENSE and NOTICE files.

//! The contract between the Bank-Node and OBP-API.
//!
//! `obp-api-contract/contract.json` lists every OBP-API call the Bank-Node
//! and its UI make, with the response fields the code reads.
//! `obp-api-contract/snapshot.json` is what an OBP-API documents for those
//! calls, written by `obp-api-contract/snapshot.py` from its resource docs.
//! These tests need no server: they check that the list matches the code, and
//! that every call reaches an endpoint OBP-API documents, with every field the
//! code reads. Retake the snapshot from a new OBP-API build to learn whether
//! that build breaks the Bank-Node.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn load(name: &str) -> Value {
    let path = workspace_root().join("obp-api-contract").join(name);
    serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap()
}

fn calls() -> Vec<Value> {
    load("contract.json")["calls"].as_array().unwrap().clone()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().map_or(vec![], |a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect())
}

fn key(call: &Value) -> String {
    format!("{} {}", call["verb"].as_str().unwrap(), call["path"].as_str().unwrap())
}

/// The snapshot entry for a call, or None.
fn documented(call: &Value, snapshot: &Value) -> Option<Value> {
    snapshot["endpoints"].as_array().unwrap().iter().find(|e| key(e) == key(call)).cloned()
}

/// The reads of `call` that `fields` does not document. A read written `a|b`
/// is satisfied by either field.
fn undocumented_reads(call: &Value, fields: &BTreeSet<String>) -> Vec<String> {
    strings(&call["reads"])
        .into_iter()
        .filter(|read| !read.split('|').any(|f| fields.contains(f)))
        .collect()
}

/// Lines of Rust source that build an OBP-API path, skipping comments and
/// test code: (file relative to the workspace, line number, line).
fn obp_path_lines() -> Vec<(String, usize, String)> {
    let root = workspace_root();
    let mut files = Vec::new();
    for krate in fs::read_dir(root.join("crates")).unwrap().flatten() {
        collect_rs(&krate.path().join("src"), &mut files);
    }
    let mut out = Vec::new();
    for file in files {
        let rel = file.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
        let name = file.file_name().unwrap().to_string_lossy().to_string();
        if name == "tests.rs" || name.ends_with("_tests.rs") {
            continue;
        }
        let text = fs::read_to_string(&file).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            // A `#[cfg(test)] mod tests { .. }` block runs to the end of the file in this workspace.
            if line.trim() == "#[cfg(test)]" && lines.get(i + 1).is_some_and(|next| next.trim().ends_with('{')) {
                break;
            }
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            if code.contains('"') && (code.contains("/obp/v") || code.contains("/obp/{")) {
                out.push((rel.clone(), i + 1, line.to_string()));
            }
        }
    }
    out
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn every_call_in_the_contract_is_in_its_source_file() {
    // Tests that each listed call still exists: every needle of an entry occurs in the entry's source file.
    let root = workspace_root();
    let mut gone = Vec::new();
    for call in calls() {
        let text = fs::read_to_string(root.join(call["source"].as_str().unwrap())).unwrap();
        for needle in strings(&call["needles"]) {
            if !text.contains(&needle) {
                gone.push(format!("{} — {needle}", key(&call)));
            }
        }
    }
    assert_eq!(gone, Vec::<String>::new());
}

#[test]
fn every_obp_api_call_in_the_source_is_in_the_contract() {
    // Tests coverage: a line that builds an OBP-API path must contain a needle of a call listed for that file,
    // so a new call cannot reach OBP-API without its fields being recorded.
    let calls = calls();
    let unlisted: Vec<String> = obp_path_lines()
        .into_iter()
        .filter(|(file, _, line)| {
            !calls.iter().any(|c| {
                c["source"].as_str() == Some(file.as_str())
                    && strings(&c["needles"]).iter().any(|n| line.contains(n.trim()))
            })
        })
        .map(|(file, n, line)| format!("{file}:{n} {}", line.trim()))
        .collect();
    assert_eq!(unlisted, Vec::<String>::new());
}

#[test]
fn the_snapshot_was_taken_for_this_contract() {
    // Tests that snapshot.json holds exactly the calls of contract.json; otherwise rerun snapshot.py.
    let snapshot = load("snapshot.json");
    let listed: BTreeSet<String> = calls().iter().map(key).collect();
    let snapped: BTreeSet<String> = snapshot["endpoints"].as_array().unwrap().iter().map(key).collect();
    assert_eq!(listed, snapped);
}

#[test]
fn every_call_reaches_an_endpoint_obp_api_documents() {
    // Tests that each call's version and path name a documented endpoint, unless the entry records why not
    // (undocumented_endpoint). A recorded gap that OBP-API has since documented fails too, so the note goes.
    let snapshot = load("snapshot.json");
    let mut problems = Vec::new();
    for call in calls() {
        let entry = documented(&call, &snapshot).unwrap_or_default();
        let missing = entry["missing"].as_bool().unwrap_or(false);
        let known_gap = call["undocumented_endpoint"].as_str();
        match (missing, known_gap) {
            (true, None) => problems.push(format!("{}: no documented endpoint", key(&call))),
            (false, Some(_)) => problems.push(format!("{}: now documented as {}; drop undocumented_endpoint", key(&call), entry["operation_id"])),
            (true, Some(why)) => eprintln!("known gap — {}: {why}", key(&call)),
            (false, None) => {}
        }
        if let Some(ops) = entry["ambiguous"].as_array() {
            problems.push(format!("{}: matches several endpoints {ops:?}", key(&call)));
        }
    }
    assert_eq!(problems, Vec::<String>::new());
}

#[test]
fn every_field_the_code_reads_is_documented() {
    // Tests that each field the code reads is in the documented response of its endpoint, unless the entry
    // lists it in undocumented_fields; a listed field that OBP-API now documents fails, so the list shrinks.
    let snapshot = load("snapshot.json");
    let mut problems = Vec::new();
    for call in calls() {
        let Some(entry) = documented(&call, &snapshot) else { continue };
        if entry["missing"].as_bool().unwrap_or(false) {
            continue;
        }
        let fields: BTreeSet<String> = strings(&entry["fields"]).into_iter().collect();
        let known: BTreeSet<String> = strings(&call["undocumented_fields"]).into_iter().collect();
        for read in undocumented_reads(&call, &fields) {
            if known.contains(&read) {
                eprintln!("known gap — {}: {read} is not documented", key(&call));
            } else {
                problems.push(format!("{} ({}): reads {read}, not documented", key(&call), entry["operation_id"]));
            }
        }
        for field in &known {
            if fields.contains(field) {
                problems.push(format!("{}: {field} is now documented; drop it from undocumented_fields", key(&call)));
            }
        }
    }
    assert_eq!(problems, Vec::<String>::new());
}

#[test]
fn a_renamed_or_removed_field_is_reported() {
    // Tests the check itself: with `conversion_value` gone from the documented fields, the FX read is reported,
    // and a read with an alternative (`transaction_request_id|id`) passes on either field.
    let fx = serde_json::json!({"reads": ["conversion_value"]});
    let renamed: BTreeSet<String> = ["bank_id", "rate"].iter().map(|s| s.to_string()).collect();
    assert_eq!(undocumented_reads(&fx, &renamed), vec!["conversion_value"]);
    let tr = serde_json::json!({"reads": ["transaction_request_id|id"]});
    let only_id: BTreeSet<String> = ["id".to_string()].into_iter().collect();
    assert!(undocumented_reads(&tr, &only_id).is_empty());
    assert_eq!(undocumented_reads(&tr, &BTreeSet::new()), vec!["transaction_request_id|id"]);
}
