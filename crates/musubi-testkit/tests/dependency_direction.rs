//! Dependency direction between workspace crates (docs/plan/phase1.md, chapter 2;
//! CONTRIBUTING.md). Runs in CI with `cargo test --workspace`.
//!
//! cargo-deny cannot express this: `[[bans.deny]] wrappers` treats dev-dependencies
//! like normal ones, and `[graph] exclude-dev` drops all dev-dependencies, so it
//! can neither allow `musubi-engine` → `musubi-store-sqlite` in tests nor forbid
//! `musubi-engine` → `musubi-google` there. This test reads `cargo metadata` instead.

use std::process::Command;

/// Internal crates each workspace crate may use in `[dependencies]` and
/// `[build-dependencies]`. Every workspace member must be listed.
const ALLOWED: &[(&str, &[&str])] = &[
    ("musubi-model", &[]),
    ("musubi-recurrence", &[]),
    ("musubi-store-sqlite", &["musubi-model"]),
    ("musubi-google", &["musubi-model"]),
    ("musubi-engine", &["musubi-model", "musubi-recurrence"]),
    (
        "musubi-syncd",
        &[
            "musubi-model",
            "musubi-recurrence",
            "musubi-store-sqlite",
            "musubi-google",
            "musubi-engine",
        ],
    ),
    ("musubi-testkit", &["musubi-model"]),
];

/// Internal crates a workspace crate must not use even in `[dev-dependencies]`.
const DEV_FORBIDDEN: &[(&str, &str)] = &[("musubi-engine", "musubi-google")];

#[derive(Debug)]
struct Dep {
    name: String,
    /// `None` for normal, `Some("dev")`, `Some("build")`.
    kind: Option<String>,
}

fn violations(members: &[(String, Vec<Dep>)]) -> Vec<String> {
    let internal = |name: &str| ALLOWED.iter().any(|(n, _)| *n == name);
    let mut out = Vec::new();
    for (pkg, deps) in members {
        let Some((_, allowed)) = ALLOWED.iter().find(|(n, _)| n == pkg) else {
            out.push(format!("{pkg}: workspace member missing from ALLOWED"));
            continue;
        };
        for d in deps.iter().filter(|d| internal(&d.name)) {
            match d.kind.as_deref() {
                Some("dev") => {
                    if DEV_FORBIDDEN.contains(&(pkg.as_str(), d.name.as_str())) {
                        out.push(format!(
                            "{pkg} -> {} (dev): forbidden even in tests",
                            d.name
                        ));
                    }
                }
                kind => {
                    if !allowed.contains(&d.name.as_str()) {
                        out.push(format!(
                            "{pkg} -> {} ({}): not allowed",
                            d.name,
                            kind.unwrap_or("normal")
                        ));
                    }
                }
            }
        }
    }
    out
}

fn workspace_members() -> Vec<(String, Vec<Dep>)> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--manifest-path",
        ])
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let members = meta["workspace_members"].as_array().unwrap();
    meta["packages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| members.contains(&p["id"]))
        .map(|p| {
            let deps = p["dependencies"]
                .as_array()
                .unwrap()
                .iter()
                .map(|d| Dep {
                    name: d["name"].as_str().unwrap().to_owned(),
                    kind: d["kind"].as_str().map(str::to_owned),
                })
                .collect();
            (p["name"].as_str().unwrap().to_owned(), deps)
        })
        .collect()
}

#[test]
fn workspace_respects_dependency_direction() {
    let members = workspace_members();
    assert_eq!(
        members.len(),
        ALLOWED.len(),
        "every crate under crates/ is a workspace member and listed in ALLOWED"
    );
    let v = violations(&members);
    assert!(
        v.is_empty(),
        "dependency direction violated (see CONTRIBUTING.md):\n{}",
        v.join("\n")
    );
}

#[test]
fn injected_violations_are_detected() {
    let dep = |name: &str, kind: Option<&str>| Dep {
        name: name.into(),
        kind: kind.map(Into::into),
    };
    let members = vec![
        (
            "musubi-engine".to_owned(),
            vec![
                dep("musubi-model", None),
                dep("musubi-store-sqlite", None),
                dep("musubi-store-sqlite", Some("dev")),
                dep("musubi-testkit", Some("dev")),
                dep("musubi-google", Some("dev")),
                dep("serde", None),
            ],
        ),
        (
            "musubi-model".to_owned(),
            vec![dep("musubi-testkit", Some("build"))],
        ),
        ("musubi-new".to_owned(), vec![]),
    ];
    assert_eq!(
        violations(&members),
        [
            "musubi-engine -> musubi-store-sqlite (normal): not allowed",
            "musubi-engine -> musubi-google (dev): forbidden even in tests",
            "musubi-model -> musubi-testkit (build): not allowed",
            "musubi-new: workspace member missing from ALLOWED",
        ]
    );
}
