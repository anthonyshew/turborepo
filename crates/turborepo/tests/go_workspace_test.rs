//! End-to-end coverage for experimental Go module/workspace support.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

mod common;

use std::{fs, path::Path, process::Command};

use common::setup;

fn go_available() -> bool {
    Command::new("go")
        .arg("version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn setup_fixture(name: &str, dir: &Path) {
    setup::copy_fixture(name, dir).unwrap();
    setup::setup_git(dir).unwrap();
}

fn run_turbo(dir: &Path, args: &[&str]) -> std::process::Output {
    let config = tempfile::tempdir().unwrap();
    common::turbo_command(dir)
        .env("GOTOOLCHAIN", "local")
        .env("TURBO_CONFIG_DIR_PATH", config.path())
        .args(args)
        .output()
        .expect("turbo runs")
}

fn assert_success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn build_hash(dir: &Path, package: &str) -> String {
    let filter = format!("--filter={package}");
    let output = run_turbo(dir, &["build", &filter, "--dry-run=json"]);
    assert_success(&output);
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let task_id = format!("{package}#build");
    json["tasks"]
        .as_array()
        .and_then(|tasks| tasks.iter().find(|task| task["taskId"] == task_id))
        .and_then(|task| task["hash"].as_str())
        .unwrap()
        .to_string()
}

#[test]
fn pure_go_workspace_discovers_graph_and_native_tasks() {
    if !go_available() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    setup_fixture("go_pure_workspace", temp.path());

    let output = run_turbo(temp.path(), &["build", "--dry-run=json"]);
    assert_success(&output);
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        json["packages"],
        serde_json::json!(["example.com/api", "example.com/auth"])
    );
    let tasks = json["tasks"].as_array().unwrap();
    let api = tasks
        .iter()
        .find(|task| task["taskId"] == "example.com/api#build")
        .unwrap();
    assert_eq!(api["command"], "go build ./...");
    assert!(api["inputs"].get("../../packages/auth/auth.go").is_some());
    assert!(!temp.path().join("package.json").exists());

    let output = run_turbo(
        temp.path(),
        &["build", "--filter=example.com/auth", "--dry-run=json"],
    );
    assert_success(&output);
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["tasks"][0]["command"], "go build ./...");

    let output = run_turbo(
        temp.path(),
        &[
            "query",
            "query { package(name: \"example.com/api\") { directDependencies { items { name } } } \
             }",
        ],
    );
    assert_success(&output);
    let query: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        query["data"]["package"]["directDependencies"]["items"][0]["name"],
        "example.com/auth"
    );

    let output = run_turbo(
        temp.path(),
        &["run", "run", "--filter=example.com/api", "--", "hello"],
    );
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("hello from Go"));
}

#[test]
fn root_go_mod_is_a_single_module_workspace() {
    if !go_available() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    setup_fixture("go_single_module", temp.path());
    let output = run_turbo(temp.path(), &["run", "run", "--dry-run=json"]);
    assert_success(&output);
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["packages"], serde_json::json!(["example.com/single"]));
    assert_eq!(json["tasks"][0]["command"], "go run example.com/single");
}

#[test]
fn mixed_javascript_and_go_packages_compose() {
    if !go_available() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    setup_fixture("go_monorepo", temp.path());
    let output = run_turbo(temp.path(), &["build", "--dry-run=json"]);
    assert_success(&output);
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let packages = json["packages"].as_array().unwrap();
    for package in ["example.com/api", "example.com/auth", "js-pkg"] {
        assert!(packages.iter().any(|value| value == package), "{package}");
    }
}

#[test]
fn affected_propagates_across_local_module_edges() {
    if !go_available() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    setup_fixture("go_pure_workspace", temp.path());
    fs::write(
        temp.path().join("packages/auth/auth.go"),
        "package auth\nfunc Greeting() string { return \"affected\" }\n",
    )
    .unwrap();
    let output = run_turbo(temp.path(), &["ls", "--affected"]);
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("example.com/auth"));
    assert!(stdout.contains("example.com/api"));
}

#[test]
fn dependency_source_changes_invalidate_dependent_module() {
    if !go_available() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    setup_fixture("go_pure_workspace", temp.path());
    let before = build_hash(temp.path(), "example.com/api");
    fs::write(
        temp.path().join("packages/auth/auth.go"),
        "package auth\nfunc Greeting() string { return \"changed\" }\n",
    )
    .unwrap();
    let after = build_hash(temp.path(), "example.com/api");
    assert_ne!(before, after);
}

#[test]
fn prune_keeps_local_module_closure_and_buildable_workspace() {
    if !go_available() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    setup_fixture("go_pure_workspace", temp.path());
    let output = run_turbo(temp.path(), &["prune", "example.com/api"]);
    assert_success(&output);
    let out = temp.path().join("out");
    for path in [
        "go.work",
        "apps/api/go.mod",
        "apps/api/main.go",
        "packages/auth/go.mod",
        "packages/auth/auth.go",
    ] {
        assert!(out.join(path).exists(), "missing {path}");
    }
    let output = Command::new("go")
        .env("GOTOOLCHAIN", "local")
        .args(["run", "./apps/api"])
        .current_dir(out)
        .output()
        .unwrap();
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("hello from Go"));
}
