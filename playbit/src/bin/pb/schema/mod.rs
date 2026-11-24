//! Local docdb schema generation. Process one bounded input at a time, before
//! opening its output so invalid schemas leave existing bindings intact.

mod generator;

use playbit::engine::Result;
use std::{fs, io::Read, path::Path};

// Developer schemas fit within 1 MiB; bound reads even if the file grows.
const MAX_SCHEMA_BYTES: u64 = 1024 * 1024;

pub fn generate(arguments: &[String]) -> Result<()> {
    if arguments.is_empty() {
        return Err(crate::usage_error("expected pb schema-gen SCHEMA.json..."));
    }
    for argument in arguments {
        let path = Path::new(argument);
        if path.extension().is_none_or(|extension| extension != "json") {
            return Err(crate::usage_error(format!(
                "schema path must end in .json: {}",
                path.display()
            )));
        }
        generate_file(path).map_err(|error| format!("schema {}: {error}", path.display()))?;
    }
    Ok(())
}

fn generate_file(path: &Path) -> Result<()> {
    let mut input = String::new();
    fs::File::open(path)?.take(MAX_SCHEMA_BYTES + 1).read_to_string(&mut input)?;
    if input.len() as u64 > MAX_SCHEMA_BYTES {
        return Err("schema exceeds 1 MiB".into());
    }
    let output = generator::process_schema(Ok(input))?;
    let output_path = path.with_extension("rs");
    fs::write(&output_path, output)
        .map_err(|error| format!("write {}: {error}", output_path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_each_requested_schema() {
        // Exercise multiple paths, including spaces, with the library's schema.
        let directory = tempfile::tempdir().expect("temporary directory");
        let source = include_str!("../../../../docdb/types.json");
        let paths: Vec<_> = ["first.json", "second schema.json"]
            .iter()
            .map(|name| directory.path().join(name))
            .collect();
        for path in &paths {
            fs::write(path, source).expect("schema");
        }
        let arguments: Vec<_> = paths.iter().map(|path| path.display().to_string()).collect();
        generate(&arguments).expect("generate bindings");
        let expected = generator::process_schema(Ok(source.into())).expect("valid schema");
        for path in paths {
            assert_eq!(fs::read_to_string(path.with_extension("rs")).expect("bindings"), expected);
        }
    }

    #[test]
    fn reports_errors_and_preserves_bindings_for_invalid_input() {
        // Invalid input must fail before replacing an existing generated file.
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("types.json");
        let arguments = [path.display().to_string()];
        assert!(generate(&[]).is_err());
        assert!(generate(&["types.rs".into()]).is_err());
        assert!(generate(&arguments).is_err());
        fs::write(path.with_extension("rs"), "preserved").expect("existing bindings");
        for input in ["{", "{}", "[{}]"] {
            fs::write(&path, input).expect("invalid schema");
            assert!(generate(&arguments).is_err());
            assert_eq!(
                fs::read_to_string(path.with_extension("rs")).expect("bindings"),
                "preserved"
            );
        }
        fs::File::create(&path).expect("schema").set_len(MAX_SCHEMA_BYTES + 1).expect("oversize");
        assert!(generate(&arguments).is_err());
        fs::write(&path, "[]").expect("valid schema");
        fs::remove_file(path.with_extension("rs")).expect("remove bindings");
        fs::create_dir(path.with_extension("rs")).expect("unwritable output");
        assert!(generate(&arguments).is_err());
    }
}
