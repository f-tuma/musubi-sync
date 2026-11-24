//! Freeze of the adapter contract v0 (docs/plan/phase2.md, chapter 3.8).
//!
//! `docs/contract/v0.md` carries a SHA-256 per contract module of
//! `musubi-model`. Any change to a module's code fails this test until the same
//! PR updates `v0.md`: an additive change records a v0.x entry, a breaking one
//! starts `v1.md`.
//!
//! Normalised text, so documentation alone does not count as a change: the
//! module up to its trailing `#[cfg(test)]` block, without lines that are only
//! a comment (`//`, `///`, `//!`), without blank lines, each line trimmed at the
//! end, joined by `\n`.

use std::collections::BTreeMap;
use std::path::Path;

use sha2::{Digest, Sha256};

/// The contract modules of chapter 3.8, relative to the workspace root.
const MODULES: &[&str] = &[
    "crates/musubi-model/src/provider.rs",
    "crates/musubi-model/src/capability.rs",
    "crates/musubi-model/src/remote.rs",
    "crates/musubi-model/src/kind.rs",
    "crates/musubi-model/src/error.rs",
    "crates/musubi-model/src/id.rs",
    "crates/musubi-model/src/time.rs",
    "crates/musubi-model/src/event.rs",
    "crates/musubi-model/src/task.rs",
    "crates/musubi-model/src/contact.rs",
];

const DOC: &str = "docs/contract/v0.md";

fn normalise(src: &str) -> String {
    src.lines()
        .take_while(|l| !l.starts_with("#[cfg(test)]"))
        .map(str::trim_end)
        .filter(|l| !l.is_empty() && !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Rows `| \`path\` | \`sha256\` |` of the hash table in `v0.md`.
fn recorded(doc: &str) -> BTreeMap<String, String> {
    doc.lines()
        .filter_map(|l| {
            let cells: Vec<_> = l.split('|').map(|c| c.trim().trim_matches('`')).collect();
            match cells.as_slice() {
                ["", path, hash, ""] if path.ends_with(".rs") => {
                    Some(((*path).to_owned(), (*hash).to_owned()))
                }
                _ => None,
            }
        })
        .collect()
}

#[test]
fn contract_modules_match_v0() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let doc = std::fs::read_to_string(root.join(DOC)).expect("read docs/contract/v0.md");
    let recorded = recorded(&doc);
    let actual: BTreeMap<String, String> = MODULES
        .iter()
        .map(|m| {
            let src = std::fs::read_to_string(root.join(m)).unwrap_or_else(|e| panic!("{m}: {e}"));
            ((*m).to_owned(), hex(&Sha256::digest(normalise(&src))))
        })
        .collect();
    let table: String = actual
        .iter()
        .map(|(m, h)| format!("| `{m}` | `{h}` |\n"))
        .collect();
    assert_eq!(
        recorded, actual,
        "the contract changed: record it in {DOC} (chapter 3.8) and replace the hash table with:\n{table}"
    );
}

#[test]
fn normalise_ignores_comments_blank_lines_and_tests() {
    let a = "//! module\npub struct A; // kept\n\n/// doc\npub fn f() {}  \n#[cfg(test)]\nmod tests {}\n";
    let b = "pub struct A; // kept\npub fn f() {}";
    assert_eq!(normalise(a), b);
    assert_ne!(normalise("pub struct A;"), normalise("pub struct B;"));
}
