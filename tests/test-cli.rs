// These tests largely just check that basic CLI configs still work,
// and show you how you've changed the output. Yes, a lot of these
// will randomly churn (especially the help message ones, which
// contain the latest version), but this is good for two reasons:
//
// * You can easily see exactly what you changed
// * It can prompt you to update any other docs
//
// `cargo insta` automates reviewing and updating these snapshots.
// You can install `cargo insta` with:
//
// > cargo install cargo-insta
//
// Also note that `cargo test` for an application adds our binary to
// the env as `CARGO_BIN_EXE_<name>`.

use std::{
    path::PathBuf,
    process::{Command, Output, Stdio},
};

// Some tests need to write files (and read them back).
// To keep this tidy and hidden, we make a new directory
// in `target`.
// const TEST_TMP: &str = "../target/testdata/";

fn format_outputs(output: &Output) -> String {
    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    // NOTE: We filter out the "Blocking: waiting for file lock" lines, as they
    // are printed out non-deterministically when there is file contention.
    let stderr = std::str::from_utf8(&output.stderr)
        .unwrap()
        .lines()
        .filter(|line| !line.starts_with("Blocking: waiting for file lock"))
        .collect::<String>();
    format!("stdout:\n{stdout}\nstderr:\n{stderr}")
}

#[test]
fn test_version() {
    let bin = env!("CARGO_BIN_EXE_cargo-vet");
    let output = Command::new(bin)
        .arg("vet")
        .arg("-V")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert!(output.status.success(), "{}", stderr);
    assert_eq!(stderr, "");

    let (name, ver) = stdout.split_once(' ').unwrap();
    assert_eq!(name, "cargo-vet");
    let mut ver_parts = ver.trim().split('.');
    ver_parts.next().unwrap().parse::<u8>().unwrap();
    ver_parts.next().unwrap().parse::<u8>().unwrap();
    ver_parts.next().unwrap().parse::<u8>().unwrap();
    assert!(ver_parts.next().is_none());
}

#[test]
fn test_long_help() {
    let bin = env!("CARGO_BIN_EXE_cargo-vet");
    let output = Command::new(bin)
        .arg("vet")
        .arg("--help")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    insta::assert_snapshot!("long-help", format_outputs(&output));
    assert!(output.status.success(), "{}", output.status);
}

#[test]
fn test_short_help() {
    let bin = env!("CARGO_BIN_EXE_cargo-vet");
    let output = Command::new(bin)
        .arg("vet")
        .arg("-h")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    insta::assert_snapshot!("short-help", format_outputs(&output));
    assert!(output.status.success(), "{}", output.status);
}

#[test]
fn test_markdown_help() {
    let bin = env!("CARGO_BIN_EXE_cargo-vet");
    let output = Command::new(bin)
        .arg("vet")
        .arg("help-markdown")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    insta::assert_snapshot!("markdown-help", format_outputs(&output));
    assert!(output.status.success(), "{}", output.status);
}

#[test]
fn test_project() {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("test-project");
    let bin = env!("CARGO_BIN_EXE_cargo-vet");
    let output = Command::new(bin)
        .current_dir(&project)
        .arg("vet")
        .arg("--manifest-path")
        .arg("Cargo.toml")
        .arg("--diff-cache")
        .arg("../diff-cache.toml")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    insta::assert_snapshot!("test-project", format_outputs(&output));
    assert!(output.status.success(), "{}", output.status);
}

#[test]
fn test_project_json() {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("test-project");
    let bin = env!("CARGO_BIN_EXE_cargo-vet");
    let output = Command::new(bin)
        .current_dir(&project)
        .arg("vet")
        .arg("--manifest-path")
        .arg("Cargo.toml")
        .arg("--diff-cache")
        .arg("../diff-cache.toml")
        .arg("--output-format=json")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    insta::assert_snapshot!("test-project-json", format_outputs(&output));
    assert!(output.status.success(), "{}", output.status);
}

#[test]
fn test_project_suggest() {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("test-project");
    let bin = env!("CARGO_BIN_EXE_cargo-vet");
    let output = Command::new(bin)
        .current_dir(&project)
        .arg("vet")
        .arg("--diff-cache")
        .arg("../diff-cache.toml")
        .arg("--manifest-path")
        .arg("Cargo.toml")
        .arg("suggest")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    insta::assert_snapshot!("test-project-suggest", format_outputs(&output));
    assert!(output.status.success(), "{}", output.status);
}

#[test]
fn test_project_suggest_json() {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("test-project");
    let bin = env!("CARGO_BIN_EXE_cargo-vet");
    let output = Command::new(bin)
        .current_dir(&project)
        .arg("vet")
        .arg("--diff-cache")
        .arg("../diff-cache.toml")
        .arg("--manifest-path")
        .arg("Cargo.toml")
        .arg("--output-format=json")
        .arg("suggest")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    insta::assert_snapshot!("test-project-suggest-json", format_outputs(&output));
    assert!(output.status.success(), "{}", output.status);
}

#[test]
fn test_project_suggest_deeper() {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("test-project");
    let bin = env!("CARGO_BIN_EXE_cargo-vet");
    let output = Command::new(bin)
        .current_dir(&project)
        .arg("vet")
        .arg("--diff-cache")
        .arg("../diff-cache.toml")
        .arg("--manifest-path")
        .arg("Cargo.toml")
        .arg("suggest")
        .arg("--guess-deeper")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    insta::assert_snapshot!("test-project-suggest-deeper", format_outputs(&output));
    assert!(output.status.success(), "{}", output.status);
}

#[test]
fn test_project_suggest_deeper_json() {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("test-project");
    let bin = env!("CARGO_BIN_EXE_cargo-vet");
    let output = Command::new(bin)
        .current_dir(&project)
        .arg("vet")
        .arg("--diff-cache")
        .arg("../diff-cache.toml")
        .arg("--manifest-path")
        .arg("Cargo.toml")
        .arg("--output-format=json")
        .arg("suggest")
        .arg("--guess-deeper")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    insta::assert_snapshot!("test-project-suggest-deeper-json", format_outputs(&output));
    assert!(output.status.success(), "{}", output.status);
}

#[test]
fn test_project_dump_graph_full_json() {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("test-project");
    let bin = env!("CARGO_BIN_EXE_cargo-vet");
    let output = Command::new(bin)
        .current_dir(&project)
        .arg("vet")
        .arg("--diff-cache")
        .arg("../diff-cache.toml")
        .arg("--manifest-path")
        .arg("Cargo.toml")
        .arg("--output-format=json")
        .arg("dump-graph")
        .arg("--depth=full")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    insta::assert_snapshot!("test-project-dump-graph-full-json", format_outputs(&output));
    assert!(output.status.success(), "{}", output.status);
}

#[test]
fn test_project_dump_graph_full() {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("test-project");
    let bin = env!("CARGO_BIN_EXE_cargo-vet");
    let output = Command::new(bin)
        .current_dir(&project)
        .arg("vet")
        .arg("--diff-cache")
        .arg("../diff-cache.toml")
        .arg("--manifest-path")
        .arg("Cargo.toml")
        .arg("dump-graph")
        .arg("--depth=full")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    insta::assert_snapshot!("test-project-dump-graph-full", format_outputs(&output));
    assert!(output.status.success(), "{}", output.status);
}
