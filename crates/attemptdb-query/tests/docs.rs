//! The documentation index and the documents that name commands cannot drift
//! from the repository without a test noticing.

use std::path::{Path, PathBuf};

fn docs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs")
}

#[test]
fn every_document_in_docs_is_in_the_index() {
    let index = std::fs::read_to_string(docs_dir().join("README.md")).unwrap();
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(docs_dir()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "md") {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if name != "README.md" && !index.contains(&format!("({name})")) {
                missing.push(name);
            }
        }
    }
    assert!(
        missing.is_empty(),
        "docs/README.md does not link: {missing:?}"
    );
}

#[test]
fn no_document_names_a_command_that_does_not_exist() {
    // `attempt sql` was never a command: SQL goes through `attempt query`.
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "md") {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    walk(&docs_dir(), &mut files);
    files.push(docs_dir().join("../README.md"));
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        for (n, line) in text.lines().enumerate() {
            assert!(
                !line.contains("attempt sql"),
                "{}:{}: `attempt sql` is not a command (use `attempt query`): {line}",
                file.display(),
                n + 1
            );
        }
    }
}
