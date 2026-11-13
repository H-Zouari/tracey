use std::fs;
use std::path::Path;
use std::process::Command;

fn write(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("file has a parent")).expect("create parent");
    fs::write(path, content).expect("write file");
}

fn covered_rules(root: &Path) -> u64 {
    let output = Command::new(env!("CARGO_BIN_EXE_tracey"))
        .arg("query")
        .arg(root)
        .args(["--json", "--no-daemon", "status"])
        .output()
        .expect("tracey runs");
    let status: serde_json::Value = serde_json::from_slice(&output.stdout).expect("status is JSON");
    status["impls"][0]["coveredRules"]
        .as_u64()
        .expect("status reports covered rules")
}

#[test]
fn a_second_run_keeps_unchanged_files_and_rereads_edited_ones() {
    let project = tempfile::tempdir().expect("temporary project");
    let root = project.path();
    write(
        root,
        ".config/tracey/config.styx",
        "specs (\n  {\n    name test\n    include (spec.md)\n    impls (\n      {\n        name rust\n        include (src/**/*.rs)\n      }\n    )\n  }\n)\n",
    );
    write(
        root,
        "spec.md",
        "# Spec\n\nr[alpha.one]\nOne MUST hold.\n\nr[alpha.two]\nTwo MUST hold.\n",
    );
    write(
        root,
        "src/one.rs",
        "// r[impl alpha.one]\npub fn one() {}\n",
    );
    write(root, "src/two.rs", "pub fn two() {}\n");

    assert_eq!(covered_rules(root), 1);
    assert_eq!(
        fs::read_to_string(root.join(".tracey_cache/.gitignore")).expect("cache ignores itself"),
        "*\n"
    );
    assert!(
        fs::read_to_string(root.join(".tracey_cache/CACHEDIR.TAG"))
            .expect("cache is tagged")
            .starts_with("Signature: 8a477f597d28d172789f06886806bc55")
    );

    write(
        root,
        "src/two.rs",
        "// r[impl alpha.two]\npub fn two() {}\n",
    );
    assert_eq!(covered_rules(root), 2);
}
