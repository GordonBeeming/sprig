//! Explicitly installed per-user launchd job; enabling policy alone starts no jobs.
use crate::{config, git::Repository, Error, Output, Result};
use serde_json::json;
use std::{
    fs,
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};
const LABEL: &str = "com.sprig.cleanup";
fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
fn xml_path(path: &Path) -> Result<String> {
    Ok(escape(path.to_str().ok_or_else(|| {
        Error::new("E_UNSUPPORTED", "non-UTF-8 scheduler path")
    })?))
}
pub fn plist(
    binary: &Path,
    repositories: &[PathBuf],
    search_path: &str,
    home: &Path,
    log: &Path,
) -> Result<String> {
    let mut args = format!(
        "<string>{}</string><string>--json</string><string>cleanup</string><string>run</string>",
        xml_path(binary)?
    );
    for repo in repositories {
        args.push_str(&format!(
            "<string>--repository</string><string>{}</string>",
            xml_path(repo)?
        ));
    }
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>{LABEL}</string>
<key>ProgramArguments</key><array>{args}</array>
<key>EnvironmentVariables</key><dict><key>PATH</key><string>{}</string><key>HOME</key><string>{}</string></dict>
<key>StartCalendarInterval</key><dict><key>Minute</key><integer>0</integer></dict>
<key>RunAtLoad</key><true/>
<key>ProcessType</key><string>Background</string>
<key>LowPriorityIO</key><true/>
<key>StandardOutPath</key><string>{}</string>
<key>StandardErrorPath</key><string>{}</string>
</dict></plist>
"#,
        escape(search_path),
        xml_path(home)?,
        xml_path(log)?,
        xml_path(&log.with_extension("error.log"))?
    ))
}
fn path() -> Result<PathBuf> {
    let home =
        std::env::var_os("HOME").ok_or_else(|| Error::new("E_CONFIG", "HOME unavailable"))?;
    Ok(PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}
fn domain() -> String {
    format!("gui/{}", unsafe { libc::geteuid() })
}
fn loaded() -> Result<bool> {
    Ok(Command::new("/bin/launchctl")
        .args(["print", &format!("{}/{LABEL}", domain())])
        .output()?
        .status
        .success())
}
fn launch(args: &[&std::ffi::OsStr]) -> Result<()> {
    let result = Command::new("/bin/launchctl").args(args).output()?;
    if !result.status.success() {
        return Err(Error::new(
            "E_SCHEDULE",
            String::from_utf8_lossy(&result.stderr).trim().to_string(),
        ));
    }
    Ok(())
}
pub fn status() -> Result<Output> {
    if !cfg!(target_os = "macos") {
        return Err(Error::new(
            "E_UNSUPPORTED",
            "cleanup scheduling requires macOS launchd",
        ));
    }
    let path = path()?;
    let binary = if path.exists() {
        let output = Command::new("/usr/bin/plutil")
            .args(["-convert", "json", "-o", "-", "--"])
            .arg(&path)
            .output()?;
        if !output.status.success() {
            return Err(Error::new(
                "E_SCHEDULE",
                "installed launch agent is not a valid plist",
            ));
        }
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        Some(PathBuf::from(
            value["ProgramArguments"][0].as_str().ok_or_else(|| {
                Error::new("E_SCHEDULE", "launch agent is missing its executable")
            })?,
        ))
    } else {
        None
    };
    Ok(Output::success(
        json!({"path":path,"installed":path.exists(),"loaded":loaded()?,"schedule":"hourly","minute":0,"binary":binary,"binary_exists":binary.as_ref().is_some_and(|p|p.is_file())}),
    ))
}
pub fn install(repositories: &[PathBuf]) -> Result<Output> {
    if !cfg!(target_os = "macos") {
        return Err(Error::new(
            "E_UNSUPPORTED",
            "cleanup scheduling requires macOS launchd",
        ));
    }
    if repositories.is_empty() {
        return Err(Error::new(
            "E_ARGUMENT",
            "provide at least one --repository",
        ));
    }
    let mut roots = std::collections::BTreeSet::new();
    for repo in repositories {
        let r = Repository::open(repo)?;
        // Use the original checkout as a stable discovery root, never a disposable worktree.
        let original = r
            .worktrees()?
            .into_iter()
            .find(|p| Repository::open(p).is_ok_and(|w| w.admin == w.common))
            .ok_or_else(|| {
                Error::new("E_SCHEDULE", "repository needs a stable original checkout")
            })?;
        roots.insert(original);
    }
    let path = path()?;
    if path.exists() || loaded()? {
        return Err(Error::new(
            "E_EXISTS",
            "a schedule is already installed; remove it before installing a replacement",
        ));
    }
    let binary = std::env::current_exe()?.canonicalize()?;
    let logs = config::user_path()?
        .parent()
        .ok_or_else(|| Error::new("E_CONFIG", "invalid user configuration directory"))?
        .to_path_buf();
    let text = plist(
        &binary,
        &roots.into_iter().collect::<Vec<_>>(),
        &std::env::var("PATH").map_err(|_| Error::new("E_CONFIG", "PATH unavailable"))?,
        &PathBuf::from(
            std::env::var_os("HOME").ok_or_else(|| Error::new("E_CONFIG", "HOME unavailable"))?,
        ),
        &logs.join("cleanup.log"),
    )?;
    let parent = path
        .parent()
        .ok_or_else(|| Error::new("E_CONFIG", "invalid launch agent path"))?;
    fs::create_dir_all(parent)?;
    fs::create_dir_all(&logs)?;
    let m = fs::symlink_metadata(parent)?;
    if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o022 != 0 {
        return Err(Error::new("E_STATE", "unsafe LaunchAgents directory"));
    }
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(text.as_bytes())?;
    file.as_file().sync_all()?;
    file.persist_noclobber(&path)
        .map_err(|e| Error::from(e.error))?;
    // Leave the concrete plist available if launchd rejects loading it.
    launch(&[
        std::ffi::OsStr::new("bootstrap"),
        std::ffi::OsStr::new(&domain()),
        path.as_os_str(),
    ])?;
    Ok(Output::success(
        json!({"path":path,"loaded":true,"binary":binary,"schedule":"hourly","minute":0,"note":"Policies remain opt-in. Reinstall the schedule if the binary moves. Logs are written beside user config."}),
    ))
}
pub fn remove() -> Result<Output> {
    if !cfg!(target_os = "macos") {
        return Err(Error::new(
            "E_UNSUPPORTED",
            "cleanup scheduling requires macOS launchd",
        ));
    }
    let path = path()?;
    if loaded()? {
        launch(&[
            std::ffi::OsStr::new("bootout"),
            std::ffi::OsStr::new(&format!("{}/{LABEL}", domain())),
        ])?;
    }
    match fs::remove_file(&path) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    Ok(Output::success(
        json!({"path":path,"loaded":false,"installed":false}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launch_agent_is_valid_and_preserves_argument_boundaries() -> Result<()> {
        let text = plist(
            Path::new("/tmp/path & spaces/sprig"),
            &[
                PathBuf::from("/tmp/repo <one>"),
                PathBuf::from("/tmp/repo two"),
            ],
            "/opt/homebrew/bin:/usr/bin:/bin",
            Path::new("/tmp/test home"),
            Path::new("/tmp/cleanup.log"),
        )?;
        assert!(text.contains("/tmp/path &amp; spaces/sprig"));
        assert!(text.contains("/tmp/repo &lt;one&gt;"));
        assert!(text.contains(
            "<key>StartCalendarInterval</key><dict><key>Minute</key><integer>0</integer></dict>"
        ));
        assert_eq!(text.matches("<string>--repository</string>").count(), 2);
        #[cfg(target_os = "macos")]
        {
            let mut file = tempfile::NamedTempFile::new()?;
            file.write_all(text.as_bytes())?;
            let result = Command::new("/usr/bin/plutil")
                .arg("-lint")
                .arg(file.path())
                .output()?;
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stdout)
            );
        }
        Ok(())
    }
}
