use std::fs;
use std::path::{Path, PathBuf};

const REQUIRED_ADR_SECTIONS: [&str; 8] = [
    "Status: Accepted",
    "Date: ",
    "## Context",
    "## Decision",
    "## Alternatives considered",
    "## Consequences",
    "## Security and reliability",
    "## Migration and reversal",
];

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("system-tests must live two levels below the repository root")
        .to_path_buf()
}

fn read_repository_file(path: &str) -> String {
    fs::read_to_string(repository_root().join(path))
        .unwrap_or_else(|error| panic!("failed to read {path}: {error}"))
}

fn normalized_policy_text(contents: &str) -> String {
    contents
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn validate_adr(path: &str, contents: &str, decision_terms: &[&str]) -> Result<(), String> {
    for required in REQUIRED_ADR_SECTIONS {
        if !contents.contains(required) {
            return Err(format!("{path} is missing `{required}`"));
        }
    }

    let lowercase = normalized_policy_text(contents);
    for placeholder in ["planned contract", "todo", "tbd", "placeholder"] {
        if lowercase.contains(placeholder) {
            return Err(format!(
                "{path} still contains placeholder text `{placeholder}`"
            ));
        }
    }

    for term in decision_terms {
        if !lowercase.contains(&term.to_ascii_lowercase()) {
            return Err(format!("{path} is missing decision term `{term}`"));
        }
    }

    if lowercase.contains("remote execution is allowed")
        || lowercase.contains("may place orders")
        || lowercase.contains("may withdraw")
    {
        return Err(format!(
            "{path} contradicts the accepted offline/read-only boundary"
        ));
    }

    Ok(())
}

#[test]
fn accepted_adrs_have_complete_decisions_and_reversal_paths() {
    let contracts: [(&str, &[&str]); 3] = [
        (
            "docs/adr/0001-modular-monolith.md",
            &["modular monolith", "single rust daemon", "process boundary"],
        ),
        (
            "docs/adr/0002-local-data-plane.md",
            &[
                "rust data plane",
                "swift presentation layer",
                "authenticated loopback rpc",
            ],
        ),
        (
            "docs/adr/0003-no-execution-v1.md",
            &[
                "strict offline",
                "must not place orders",
                "must not withdraw",
                "read-only",
            ],
        ),
    ];

    for (path, terms) in contracts {
        let contents = read_repository_file(path);
        validate_adr(path, &contents, terms).unwrap_or_else(|error| panic!("{error}"));
    }
}

#[test]
fn adr_validation_rejects_nonaccepted_placeholder_and_contradictory_edits() {
    let path = "docs/adr/0003-no-execution-v1.md";
    let contents = read_repository_file(path);
    let terms = [
        "strict offline",
        "must not place orders",
        "must not withdraw",
        "read-only",
    ];

    let proposed = contents.replacen("Status: Accepted", "Status: Proposed", 1);
    assert!(validate_adr(path, &proposed, &terms).is_err());

    let placeholder = format!("{contents}\n\nTODO: planned contract\n");
    assert!(validate_adr(path, &placeholder, &terms).is_err());

    let contradiction = format!("{contents}\n\nRemote execution is allowed.\n");
    assert!(validate_adr(path, &contradiction, &terms).is_err());
}

#[test]
fn security_policy_names_private_reporting_triage_and_release_integrity() {
    let security = read_repository_file("SECURITY.md");
    let required = [
        "## Supported versions and channels",
        "## Private reporting",
        "## Severity and response",
        "## Coordinated disclosure and advisories",
        "## Release artifact integrity",
        "Do not open a public issue",
        "Critical",
        "High",
        "Medium",
        "Low",
        "no guaranteed response or remediation SLA",
        "affected versions",
        "patched version",
        "security advisory",
        "SHA-256",
        "signed",
    ];
    let normalized = normalized_policy_text(&security);

    for term in required {
        assert!(
            normalized.contains(&term.to_ascii_lowercase()),
            "SECURITY.md is missing required policy term `{term}`"
        );
    }
}

#[test]
fn contributing_policy_uses_dco_and_category_specific_artifact_provenance() {
    let contributing = read_repository_file("CONTRIBUTING.md");
    let required = [
        "Developer Certificate of Origin 1.1",
        "https://developercertificate.org/",
        "Signed-off-by:",
        "permanent public record",
        "fixture",
        "dataset",
        "model",
        "generated schema",
        "vendored source",
        "licenses/artifact-provenance.toml",
    ];
    let normalized = normalized_policy_text(&contributing);

    for term in required {
        assert!(
            normalized.contains(&term.to_ascii_lowercase()),
            "CONTRIBUTING.md is missing required contribution term `{term}`"
        );
    }
}
