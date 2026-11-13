//! Fleet Spec Format projects: StrictDoc specs whose nodes carry a VERSION
//! field, with code markers, `@relation` annotations and YAML evidence.

use std::path::Path;

use tracey::data::DashboardData;
use tracey_api::{ApiRule, ValidationErrorCode};

const CONFIG: &str = "specs (\n  {\n    name fleet\n    include (docs/spec/**/*.sdoc)\n    impls (\n      {\n        name rust\n        include (src/**/*.rs)\n        test_include (tests/**/*.rs docs/verification/**/*.yaml)\n      }\n    )\n  }\n)\n";

const GLOSSARY: &str = "\
[DOCUMENT]
TITLE: Glossary

[TERM]
UID: glossary.exporter
VERSION: 1
TITLE: the exporter
STATUS: active
DEFINITION: The component that writes invoice exports.
";

const EXPORT: &str = "\
[DOCUMENT]
TITLE: Export

[TEXT]
STATEMENT: Accountants export a period's invoices as one CSV file.

[FUNCTIONAL]
UID: export.file.csv
VERSION: 1
TITLE: CSV export
STATUS: active
VERIFIED_BY: test
WHEN: an accountant exports a period
SYSTEM: the exporter
KEYWORD: MUST
RESPONSE: write one CSV file with every invoice issued in that period

[FUNCTIONAL]
UID: export.file.name
VERSION: 2
TITLE: Export file name
STATUS: active
VERIFIED_BY: test
WHEN: an export file is created
SYSTEM: the exporter
KEYWORD: MUST
RESPONSE: name it <tenant>-invoices-<from>-<to>.csv
RELATIONS:
- TYPE: Parent
  VALUE: export.file.csv
  ROLE: Refines

[CONSTRAINT]
UID: export.storage.legacy
VERSION: 1
TITLE: Legacy storage
STATUS: retired
VERIFIED_BY: inspection
KEYWORD: MUST
STATEMENT: Export files MUST be stored on the legacy NAS.
";

const FLOWS: &str = "\
[DOCUMENT]
TITLE: Flows

[[FLOW]]
UID: flow.export.month
VERSION: 1
TITLE: Accountant exports a month
STATUS: active
VERIFIED_BY: test
ACTOR: accountant
MILESTONE: 2.0 Billing
OUTCOME: a CSV with the month's invoices is downloaded

[STEP]
UID: flow.export.month.s1
VERSION: 1
TITLE: Export a month
ACTION: pick a month in Exports and press Export
EXPECTED: a file downloads
RELATIONS:
- TYPE: Parent
  VALUE: export.file.csv
  ROLE: Uses

[STEP]
UID: flow.export.month.s2
VERSION: 1
TITLE: Open the file
ACTION: open the downloaded file
EXPECTED: it lists the month's invoices
RELATIONS:
- TYPE: Parent
  VALUE: export.file.name
  ROLE: Uses

[[/FLOW]]

[[FLOW]]
UID: flow.export.year
VERSION: 1
TITLE: Accountant exports a year
STATUS: active
VERIFIED_BY: test
ACTOR: accountant
MILESTONE: 2.0 Billing
OUTCOME: a CSV with the year's invoices is downloaded

[STEP]
UID: flow.export.year.s1
VERSION: 1
TITLE: Export a year
ACTION: pick a year in Exports and press Export
EXPECTED: a file downloads

[STEP]
UID: flow.export.year.s2
VERSION: 1
TITLE: Open the file
ACTION: open the downloaded file
EXPECTED: it lists the year's invoices

[[/FLOW]]
";

const EXPORT_CODE: &str = "\
// r[impl export.file.csv]
pub fn export_csv() {}

// r[impl export.file.name+1]
pub fn export_file_name() {}
";

const FLOW_TESTS: &str = "\
// r[verify flow.export.month.s1]
#[test]
fn exports_a_month() {}

// r[verify flow.export.month.s2]
#[test]
fn opens_the_month_file() {}

// r[verify flow.export.year.s1]
#[test]
fn exports_a_year() {}
";

fn write(root: &Path, files: &[(&str, &str)]) {
    for (path, content) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
}

/// The example project plus `extra` source files, loaded by a fresh engine.
async fn load(extra: &[(&str, &str)]) -> (tempfile::TempDir, std::sync::Arc<DashboardData>) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(
        root,
        &[
            (".config/tracey/config.styx", CONFIG),
            ("docs/spec/glossary.sdoc", GLOSSARY),
            ("docs/spec/export.sdoc", EXPORT),
            ("docs/spec/flows.sdoc", FLOWS),
            ("src/export.rs", EXPORT_CODE),
            ("tests/flows.rs", FLOW_TESTS),
        ],
    );
    write(root, extra);
    let engine =
        tracey::daemon::Engine::new(root.to_path_buf(), root.join(".config/tracey/config.styx"))
            .await
            .expect("engine");
    let data = engine.data().await;
    (temp, data)
}

fn rules(data: &DashboardData) -> &[ApiRule] {
    &data.forward_by_impl[&("fleet".to_string(), "rust".to_string())].rules
}

fn rule<'a>(data: &'a DashboardData, base: &str) -> &'a ApiRule {
    rules(data)
        .iter()
        .find(|r| r.id.base == base)
        .unwrap_or_else(|| panic!("{base} is a rule"))
}

fn errors_with(data: &DashboardData, code: ValidationErrorCode) -> Vec<(Option<String>, String)> {
    data.validation_by_impl()[&("fleet".to_string(), "rust".to_string())]
        .errors
        .iter()
        .filter(|e| e.code == code)
        .map(|e| (e.file.clone(), e.message.clone()))
        .collect()
}

#[tokio::test]
async fn a_marker_for_an_older_version_is_the_one_stale_reference() {
    let (_temp, data) = load(&[]).await;

    let stale: Vec<(&str, String)> = rules(&data)
        .iter()
        .flat_map(|r| {
            r.stale_refs
                .iter()
                .map(move |s| (r.id.base.as_str(), s.reference_id.to_string()))
        })
        .collect();
    assert_eq!(
        stale,
        [("export.file.name", "export.file.name".to_string())]
    );
    assert_eq!(rule(&data, "export.file.name").id.version, 2);
}

#[tokio::test]
async fn terms_and_text_are_not_rules() {
    let (_temp, data) = load(&[]).await;

    let ids: Vec<&str> = rules(&data).iter().map(|r| r.id.base.as_str()).collect();
    assert!(!ids.contains(&"glossary.exporter"), "{ids:?}");
    assert_eq!(ids.len(), 9, "{ids:?}");
}

#[tokio::test]
async fn refines_and_uses_relations_are_not_code_references() {
    let (_temp, data) = load(&[]).await;

    let csv = rule(&data, "export.file.csv");
    let files: Vec<&str> = csv.impl_refs.iter().map(|r| r.file.as_str()).collect();
    assert_eq!(files, ["src/export.rs"]);
    assert!(csv.verify_refs.is_empty());
}

#[tokio::test]
async fn a_reference_to_a_retired_rule_is_an_error() {
    let (_temp, data) = load(&[(
        "src/legacy.rs",
        "// r[impl export.storage.legacy]\npub fn store_on_nas() {}\n",
    )])
    .await;

    let retired = errors_with(&data, ValidationErrorCode::RetiredRequirement);
    assert_eq!(retired.len(), 1, "{retired:?}");
    assert_eq!(retired[0].0.as_deref(), Some("src/legacy.rs"));
    assert!(
        retired[0].1.contains("export.storage.legacy"),
        "{retired:?}"
    );
}

#[tokio::test]
async fn a_flow_is_verified_once_every_step_is() {
    let (_temp, data) = load(&[]).await;

    assert!(!rule(&data, "flow.export.month").verify_refs.is_empty());
    assert!(rule(&data, "flow.export.year").verify_refs.is_empty());
}

#[tokio::test]
async fn a_relation_annotation_to_a_rule_past_version_1_is_stale() {
    let (_temp, data) = load(&[(
        "src/relation.rs",
        "// @relation(export.file.csv, scope=function)\npub fn csv() {}\n\n// @relation(export.file.name, scope=function)\npub fn name() {}\n",
    )])
    .await;

    let stale_files = |base| -> Vec<String> {
        rule(&data, base)
            .stale_refs
            .iter()
            .map(|s| s.file.clone())
            .collect()
    };
    assert!(stale_files("export.file.csv").is_empty());
    assert!(
        stale_files("export.file.name").contains(&"src/relation.rs".to_string()),
        "{:?}",
        stale_files("export.file.name")
    );
}

#[tokio::test]
async fn a_verification_record_counts_as_a_verify_reference() {
    let (_temp, data) = load(&[(
        "docs/verification/export.file.csv.yaml",
        "# r[verify export.file.csv]\nrequirement: export.file.csv\nversion: 1\nmethod: test\n",
    )])
    .await;

    let files: Vec<String> = rule(&data, "export.file.csv")
        .verify_refs
        .iter()
        .map(|r| r.file.clone())
        .collect();
    assert_eq!(files, ["docs/verification/export.file.csv.yaml"]);
}
