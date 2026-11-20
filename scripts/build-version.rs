//! Build-time version metadata shared by the client and independently released AP.
//! Git is consulted only when the source root has its own .git entry.
use std::{io, path::Path, process::Command};

fn git(root: &Path, args: &[&str]) -> io::Result<String> {
    // Cargo already builds this source tree. Trust only this checkout for this
    // invocation, including CI containers whose user differs from the host owner.
    let root = root.canonicalize()?;
    let output = Command::new("git")
        .arg("-c")
        .arg(format!("safe.directory={}", root.display()))
        .arg("-C")
        .arg(&root)
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    String::from_utf8(output.stdout).map(|text| text.trim().to_owned()).map_err(io::Error::other)
}

pub fn full_revision(root: &Path) -> io::Result<String> {
    if !root.join(".git").exists() {
        return Ok("src".into());
    }
    let hash = git(root, &["rev-parse", "--verify", "HEAD"])?;
    if !matches!(hash.len(), 40 | 64)
        || !hash.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(io::Error::other("Git HEAD must be a full hexadecimal object ID"));
    }
    Ok(hash)
}

pub fn revision(root: &Path) -> io::Result<String> {
    let hash = full_revision(root)?;
    Ok(hash[..hash.len().min(8)].into())
}

/// Track HEAD and both loose and packed refs, including linked worktrees.
pub fn git_inputs(root: &Path) -> io::Result<Vec<String>> {
    if !root.join(".git").exists() {
        // An exported source tree has no Git inputs. Watching a missing path
        // makes Cargo rebuild the crate on every otherwise unchanged invocation.
        return Ok(Vec::new());
    }
    let mut inputs = Vec::new();
    if root.join(".git").is_file() {
        inputs.push(root.join(".git").to_string_lossy().into_owned());
    }
    for name in ["HEAD", "refs", "packed-refs"] {
        let path = git(root, &["rev-parse", "--path-format=absolute", "--git-path", name])?;
        // Packing refs changes the watched refs directory. Do not watch an absent
        // packed-refs file, which would make Cargo rerun on every invocation.
        if name != "packed-refs" || Path::new(&path).exists() {
            inputs.push(path);
        }
    }
    Ok(inputs)
}

#[cfg(not(test))]
pub fn emit(root: &Path) -> io::Result<String> {
    for input in git_inputs(root)? {
        println!("cargo:rerun-if-changed={input}");
    }
    println!("cargo:rerun-if-changed={}", root.join("scripts/build-version.rs").display());
    let release = std::env::var("CARGO_PKG_VERSION").expect("Cargo package version");
    let separator = if release.contains('+') { "." } else { "+" };
    let version = format!("{release}{separator}{}", revision(root)?);
    println!("cargo:rustc-env=PLAYBIT_BUILD_VERSION={version}");
    Ok(version)
}
