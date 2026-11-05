use anyhow::{bail, Context, Result};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

fn resolved(path: &Path) -> Result<PathBuf> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            x => normalized.push(x.as_os_str()),
        }
    }
    let mut tail = Vec::new();
    let mut parent = normalized.as_path();
    while !parent.exists() {
        tail.push(
            parent
                .file_name()
                .context("Invalid data path")?
                .to_os_string(),
        );
        parent = parent.parent().context("Invalid data parent")?;
    }
    let mut real = parent.canonicalize()?;
    for c in tail.iter().rev() {
        real.push(c);
    }
    Ok(real)
}

pub fn prepare_data_dir(path: &Path, home: &Path, extra: &[PathBuf]) -> Result<PathBuf> {
    let target = resolved(path)?;
    let home = resolved(home)?;
    if target == home || target == Path::new("/") {
        bail!("Choose a dedicated counter data directory");
    }
    let protected = [
        ".codex",
        ".claude",
        ".cursor",
        ".grokbot",
        ".grok",
        ".ai-token-league",
        "Applications",
        "Library/Keychains",
        "Library/Application Support/Claude",
        "Library/Application Support/Cursor",
        "Library/Application Support/Grok Bot",
    ];
    if target.starts_with("/Applications") {
        bail!("Counter data cannot be written inside installed applications");
    }
    let mut protected_homes = vec![home.clone()];
    if let Some(actual) = std::env::var_os("HOME") {
        let actual = resolved(&PathBuf::from(actual))?;
        if !protected_homes.contains(&actual) {
            protected_homes.push(actual);
        }
    }
    for protected_home in protected_homes {
        for relative in protected {
            let root = resolved(&protected_home.join(relative))?;
            if target.starts_with(&root) || root.starts_with(&target) {
                bail!("Data directory overlaps an AI-tool source tree");
            }
        }
    }
    for root in extra {
        let root = resolved(root)?;
        if target.starts_with(&root) || root.starts_with(&target) {
            bail!("Data directory overlaps an approved source root");
        }
    }
    if path
        .symlink_metadata()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        bail!("Data directory cannot be a symlink");
    }
    if target.exists() {
        if !target.is_dir() {
            bail!("Data path is not a directory");
        }
        #[cfg(unix)]
        if target.metadata()?.permissions().mode() & 0o077 != 0 {
            bail!("Existing data directory must be private (mode 700); choose a new dedicated directory");
        }
    } else {
        #[cfg(unix)]
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&target)?;
        #[cfg(not(unix))]
        fs::create_dir_all(&target)?;
    }
    for name in [
        "ledger.sqlite3",
        "ledger.sqlite3-wal",
        "ledger.sqlite3-shm",
        "ledger.sqlite3-journal",
    ] {
        if let Ok(m) = target.join(name).symlink_metadata() {
            if m.file_type().is_symlink() {
                bail!("Counter database files cannot be symlinks");
            }
            #[cfg(unix)]
            if m.nlink() > 1 {
                bail!("Counter database files cannot be hardlinks");
            }
        }
    }
    Ok(target)
}

pub fn constant_time_equal(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn protected_sources_cannot_be_data_directories() {
        let home = tempfile::tempdir().unwrap();
        let source = home.path().join(".codex");
        fs::create_dir(&source).unwrap();
        assert!(prepare_data_dir(&source, home.path(), &[]).is_err());
        assert!(prepare_data_dir(&source.join("data"), home.path(), &[]).is_err());
        assert!(!source.join("data").exists());
    }
    #[test]
    fn private_own_directory_only() {
        let home = tempfile::tempdir().unwrap();
        let target = home.path().join("counter-data");
        let result = prepare_data_dir(&target, home.path(), &[]).unwrap();
        assert!(result.is_dir());
        #[cfg(unix)]
        assert_eq!(
            result.metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    #[test]
    fn token_comparison() {
        assert!(constant_time_equal("abc", "abc"));
        assert!(!constant_time_equal("abc", "abd"));
        assert!(!constant_time_equal("abc", "ab"));
    }
    #[test]
    fn alternate_source_home_still_protects_real_ai_data() {
        let fixture = tempfile::tempdir().unwrap();
        if let Some(home) = std::env::var_os("HOME") {
            assert!(prepare_data_dir(
                &PathBuf::from(home).join(".ai-token-league/counter"),
                fixture.path(),
                &[]
            )
            .is_err());
        }
    }
    #[cfg(unix)]
    #[test]
    fn hardlinked_database_is_rejected() {
        let d = tempfile::tempdir().unwrap();
        let source = d.path().join("source");
        fs::write(&source, "do not change").unwrap();
        let target = d.path().join("counter");
        fs::DirBuilder::new().mode(0o700).create(&target).unwrap();
        fs::hard_link(&source, target.join("ledger.sqlite3")).unwrap();
        assert!(prepare_data_dir(&target, d.path(), &[]).is_err());
        assert_eq!(fs::read_to_string(source).unwrap(), "do not change");
    }
}
