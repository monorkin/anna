//! Files only their owner should be able to touch.
//!
//! Anna's config decides who she listens to and which commands she runs as
//! MCP servers, and her style and personality files go straight into prompts.
//! Anyone who can write one of them controls her, so they are written private
//! and refused if they are found otherwise — loudly, because a config that
//! someone else could edit is not a config to quietly carry on with.

use anyhow::{Context, Result, bail};
use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

const OTHERS_CAN_WRITE: u32 = 0o022;
const OTHERS_CAN_DO_ANYTHING: u32 = 0o077;

/// Writes through a temp file, so a crash never leaves half a config, and
/// with the final permissions from the first byte.
pub fn write_private(path: &Path, contents: &str) -> Result<()> {
    let directory = path.parent().context("the file has no folder")?;
    fs::DirBuilder::new().recursive(true).mode(0o700).create(directory)?;

    let temporary = path.with_extension("writing");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    std::io::Write::write_all(&mut file, contents.as_bytes())?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    fs::rename(&temporary, path)?;
    Ok(())
}

/// For files that hold tokens: nobody else may even read them.
pub fn read_private(path: &Path) -> Result<String> {
    refuse_if(path, OTHERS_CAN_DO_ANYTHING, "readable or writable by others")?;
    Ok(fs::read_to_string(path)?)
}

/// For files that steer Anna: nobody else may write them.
pub fn read_trusted(path: &Path) -> Result<String> {
    refuse_if(path, OTHERS_CAN_WRITE, "writable by others")?;
    Ok(fs::read_to_string(path)?)
}

fn refuse_if(path: &Path, forbidden: u32, problem: &str) -> Result<()> {
    let metadata = fs::metadata(path).with_context(|| format!("could not read {}", path.display()))?;

    if metadata.uid() != unsafe { libc::getuid() } {
        bail!("{} belongs to someone else; refusing to use it", path.display());
    }
    if metadata.mode() & forbidden != 0 {
        bail!(
            "{} is {problem}; refusing to use it. Fix it with: chmod 600 {}",
            path.display(),
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_files_are_written_private_and_refused_when_they_are_not() {
        let directory = std::env::temp_dir().join(format!("anna-fsutil-{}", std::process::id()));
        let path = directory.join("nested/config.json");

        write_private(&path, "{}").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o777, 0o700);
        assert_eq!(read_private(&path).unwrap(), "{}");
        assert!(!path.with_extension("writing").exists());

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_private(&path).unwrap_err().to_string().contains("chmod 600"));
        assert_eq!(read_trusted(&path).unwrap(), "{}");

        fs::set_permissions(&path, fs::Permissions::from_mode(0o664)).unwrap();
        assert!(read_trusted(&path).unwrap_err().to_string().contains("writable by others"));

        fs::remove_dir_all(directory).unwrap();
    }
}
