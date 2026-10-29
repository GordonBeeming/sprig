#![cfg(target_os = "macos")]
use std::{fs, os::unix::fs::PermissionsExt, process::Command};
use tool::{platform, Result};

#[test]
fn real_apfs_metadata_and_swap() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let donor = dir.path().join("donor");
    let target = dir.path().join("target");
    let temp = dir.path().join("temp");
    fs::write(&donor, b"identical contents")?;
    fs::write(&target, b"identical contents")?;
    xattr::set(&donor, "test.donor", b"must disappear")?;
    xattr::set(&target, "test.target", b"must remain")?;
    fs::set_permissions(&target, fs::Permissions::from_mode(0o750))?;
    assert!(Command::new("chmod")
        .args([
            "+a",
            "everyone allow read,readattr,readextattr,readsecurity"
        ])
        .arg(&target)
        .status()?
        .success());
    let before = platform::fingerprint(&target)?;
    platform::clone(&donor, &temp)?;
    platform::metadata(&target, &temp)?;
    let new = platform::fingerprint(&temp)?;
    assert_eq!(before.atime, new.atime);
    assert_eq!(before.birth, new.birth);
    assert!(
        before.equivalent(&new),
        "target metadata did not round-trip:\n{before:#?}\n{new:#?}"
    );
    assert_eq!(platform::clone_id(&donor)?, platform::clone_id(&temp)?);
    platform::swap(&target, &temp)?;
    assert!(before.equivalent(&platform::fingerprint(&target)?));
    fs::write(&target, b"target writes")?;
    assert_eq!(fs::read(&donor)?, b"identical contents");
    fs::write(&donor, b"donor writes")?;
    assert_eq!(fs::read(&temp)?, b"identical contents");
    Ok(())
}
