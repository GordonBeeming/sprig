#![cfg(target_os = "macos")]
use sprig::{platform, Result};
use std::{fs, os::unix::fs::PermissionsExt, process::Command};

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

#[test]
fn clone_verification_allows_system_provenance_to_change() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("file");
    fs::write(&path, b"unchanged tracked contents")?;
    let mut source = platform::fingerprint(&path)?;
    let mut clone = source.clone();
    source.xattrs.insert(
        "com.apple.provenance".into(),
        "010200A6E5DFAA4D6AF172".into(),
    );
    clone.xattrs.insert(
        "com.apple.provenance".into(),
        "010200B3290DAE49F14B0C".into(),
    );
    assert!(
        source.preserves(&clone),
        "OS provenance must not reject otherwise preserved clone metadata"
    );
    assert!(source.preservation_differences(&clone).is_empty());
    assert!(
        !source.unchanged(&clone),
        "source mutation detection must still compare provenance"
    );
    clone.xattrs.remove("com.apple.provenance");
    assert!(
        source.preserves(&clone),
        "OS provenance may also be absent on a clone"
    );
    clone.xattrs.insert(
        "com.apple.quarantine".into(),
        "quarantine remains protected".into(),
    );
    assert!(
        !source.preserves(&clone),
        "other xattrs must remain protected"
    );
    assert!(source
        .preservation_differences(&clone)
        .contains_key("xattr com.apple.quarantine"));
    clone.xattrs.remove("com.apple.quarantine");
    clone.digest = "different bytes".into();
    assert!(
        !source.preserves(&clone),
        "provenance policy must not mask changed contents"
    );
    Ok(())
}
