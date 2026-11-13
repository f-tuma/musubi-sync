//! The codecs never serialize a whole parsed object (plan K2, chapter 3.3):
//! the raw body stays byte-for-byte as received, and re-serializing an
//! `ICalendar` or `VCard` through calcard would normalize it. A grep over the
//! sources, because calcard writes objects through `Display`/`to_string` and
//! `write_to`; `format!` of the object by name is caught too.

const BANNED: &[&str] = &[
    "to_string(",
    ".write_to(",
    "impl Display for",
    "impl fmt::Display",
    // `format!` of a parsed object goes through the same `Display`.
    "{cal",
    "{card",
    "{vcard",
];

fn sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_whole_object_serialization() {
    let mut files = Vec::new();
    sources(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    assert!(files.len() >= 4, "found the sources: {files:?}");
    let mut hits = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap();
        for (n, line) in text.lines().enumerate() {
            for banned in BANNED {
                if line.contains(banned) {
                    hits.push(format!("{}:{}: {}", file.display(), n + 1, line.trim()));
                }
            }
        }
    }
    assert!(
        hits.is_empty(),
        "whole-object serialization:\n{}",
        hits.join("\n")
    );
}
