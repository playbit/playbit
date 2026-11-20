//! Register the installed skill without replacing a user's own skill directory.
use std::{fs, io, os::unix::fs::symlink, path::Path};

pub(super) fn install() {
    let result = (|| -> io::Result<()> {
        let executable = std::env::current_exe()?.canonicalize()?;
        let Some(contents) = executable.parent().and_then(Path::parent) else {
            return Ok(());
        };
        let skill = contents.join("Resources/skills/playbit");
        // Unpackaged contributor executables have no skill to register.
        if !skill.join("SKILL.md").is_file() {
            return Ok(());
        }
        let home = std::env::var_os("HOME")
            .ok_or_else(|| io::Error::other("HOME is unavailable for skill registration"))?;
        register(Path::new(&home), &skill)
    })();
    if let Err(error) = result {
        crate::diagnostic!("Cannot register the Playbit agent skill: {error}");
    }
}

fn register(home: &Path, skill: &Path) -> io::Result<()> {
    let mut result = link(&home.join(".agents/skills/playbit"), skill);
    if home.join(".claude").is_dir() {
        let claude = link(&home.join(".claude/skills/playbit"), skill);
        if result.is_ok() {
            result = claude;
        }
    }
    result
}

fn link(path: &Path, skill: &Path) -> io::Result<()> {
    let parent = path.parent().expect("skill link has a parent");
    fs::create_dir_all(parent)?;
    match fs::read_link(path) {
        Ok(target) if target == skill => return Ok(()),
        Ok(target) if target.ends_with("Contents/Resources/skills/playbit") => {
            // Only replace links owned by a Playbit installation. A renamed or
            // moved app repairs its registration on the next ordinary launch.
            fs::remove_file(path)?;
        }
        Ok(_) => {
            return Err(io::Error::other(format!(
                "{} points to another skill; keep it or move it aside to use Playbit's skill",
                path.display()
            )));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    symlink(skill, path)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful temporary filesystem operations.")]
mod tests {
    use super::*;

    #[test]
    fn installs_repairs_and_preserves_user_skills() {
        // Exercise the real symlink lifecycle in an isolated home, including
        // relocation, missing targets, opt-in Claude discovery and conflicts.
        let home = tempfile::tempdir().unwrap();
        let first = Path::new("/Applications/Playbit.app/Contents/Resources/skills/playbit");
        let moved = Path::new("/Applications/Playbit Beta.app/Contents/Resources/skills/playbit");
        let agents = home.path().join(".agents/skills/playbit");
        register(home.path(), first).unwrap();
        assert_eq!(fs::read_link(&agents).unwrap(), first);
        assert!(!home.path().join(".claude").exists());
        register(home.path(), first).unwrap();
        fs::create_dir(home.path().join(".claude")).unwrap();
        register(home.path(), moved).unwrap();
        assert_eq!(fs::read_link(&agents).unwrap(), moved);
        assert_eq!(fs::read_link(home.path().join(".claude/skills/playbit")).unwrap(), moved);
        fs::remove_file(&agents).unwrap();
        fs::create_dir(&agents).unwrap();
        fs::write(agents.join("SKILL.md"), "My skill").unwrap();
        assert!(register(home.path(), first).is_err());
        assert_eq!(fs::read_to_string(agents.join("SKILL.md")).unwrap(), "My skill");
        fs::remove_dir_all(&agents).unwrap();
        symlink("/my/skill", &agents).unwrap();
        assert!(register(home.path(), first).is_err());
        assert_eq!(fs::read_link(&agents).unwrap(), Path::new("/my/skill"));
    }
}
