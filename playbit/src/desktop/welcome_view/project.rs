//! Initialize a source project with the CLI shipped alongside Playbit.
use crate::engine::{Result, profile};
use std::{
    env,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub(super) fn initialize(directory: &Path) -> std::result::Result<PathBuf, String> {
    let initialize = || -> Result<PathBuf> {
        let mut pb = Command::new(env::current_exe()?.with_file_name("pb"));
        profile::configure_relaunch(&mut pb)?;
        // Use the exact open profile, including profiles selected by an installation default.
        pb.env("PLAYBIT_PROFILE", profile::current().0);
        run_init(pb, directory)
    };
    initialize().map_err(|error| format!("Could not set up the project. {error}"))
}

fn run_init(mut pb: Command, directory: &Path) -> Result<PathBuf> {
    let mut child = pb
        .arg("init")
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stderr = child.stderr.take().ok_or("Could not capture pb init diagnostics")?;
    // Keep enough text for a useful error and continue draining so the child can exit.
    let mut details = Vec::new();
    let captured = stderr
        .by_ref()
        .take(4096)
        .read_to_end(&mut details)
        .and_then(|_| io::copy(&mut stderr, &mut io::sink()));
    drop(stderr);
    let status = child.wait()?;
    captured?;
    if !status.success() {
        let details = String::from_utf8_lossy(&details);
        if let Some((_, error)) = details.lines().find_map(|line| line.split_once("pb: error: ")) {
            return Err(error.into());
        }
        return Err(format!("pb init exited with {status}.").into());
    }
    let readme = directory.join("README.md");
    if !readme.is_file() {
        return Err("The project folder has no README.md to show in Finder.".into());
    }
    Ok(readme)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "The test owns the temporary shell and directories.")]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn initialization_returns_the_readme_in_the_selected_folder_only_on_success() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("my project '$(exit 99)'");
        fs::create_dir(&directory).unwrap();
        let mut pb = Command::new("/bin/sh");
        pb.args(["-c", r#"[ "$1" = init ] && printf '# Project\n' > README.md"#, "pb"]);
        let readme = run_init(pb, &directory).unwrap();
        assert_eq!(readme, directory.join("README.md"));
        assert_eq!(fs::read_to_string(&readme).unwrap(), "# Project\n");

        let mut pb = Command::new("/bin/sh");
        pb.args([
            "-c",
            "printf 'pb: error: Could not initialize this folder.\\n' >&2; exit 1",
            "pb",
        ]);
        assert_eq!(
            run_init(pb, &directory).unwrap_err().to_string(),
            "Could not initialize this folder."
        );
    }
}
