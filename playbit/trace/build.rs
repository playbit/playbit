//! Generate the bounded native name catalog from public source inputs only.
//! AP supplies its own additional catalog at bootstrap; public builds never read service/.
use std::{collections::BTreeSet, env, fs, path::Path};

fn visit(path: &Path, names: &mut BTreeSet<String>) {
    if path.is_dir() {
        let mut children: Vec<_> = fs::read_dir(path)
            .expect("read trace catalog directory")
            .map(|entry| entry.expect("catalog directory entry").path())
            .collect();
        children.sort();
        for child in children {
            visit(&child, names);
        }
        return;
    }
    if path.extension().is_none_or(|ext| ext != "rs") {
        return;
    }
    println!("cargo:rerun-if-changed={}", path.display());
    let text = fs::read_to_string(path).expect("read trace source");
    // Catalog all dotted source literals, including conditional stage choices
    // and GPUI's small wrapper. No runtime/user string registers a native name.
    for candidate in text.split('"') {
        if candidate.contains('.')
            && candidate.starts_with(|c: char| c.is_ascii_lowercase())
            && candidate
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'.')
        {
            assert!(
                candidate.len() <= 48,
                "native trace catalog name exceeds 48 bytes: {candidate}"
            );
            names.insert(candidate.to_owned());
        }
    }
}

fn main() {
    let manifest = env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory");
    let root = Path::new(&manifest).join("../..");
    let mut names = BTreeSet::new();
    for directory in ["playbit/src", "playbit/trace/src", "vendor/gpui/src"] {
        let directory = root.join(directory);
        assert!(
            directory.is_dir(),
            "native trace catalog directory is missing: {}",
            directory.display()
        );
        println!("cargo:rerun-if-changed={}", directory.display());
        visit(&directory, &mut names);
    }
    assert!(!names.is_empty(), "native trace catalog is empty");
    assert!(names.len() <= 1024, "native trace catalog exceeds 1024 names");
    let body = names.iter().map(|name| format!("    {name:?},\n")).collect::<String>();
    let source = format!("pub static NATIVE_NAMES: &[&str] = &[\n{body}];\n");
    fs::write(
        Path::new(&env::var_os("OUT_DIR").expect("Cargo output directory")).join("catalog.rs"),
        source,
    )
    .expect("write generated trace catalog");
}
