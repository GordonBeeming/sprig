//! Native filesystem boundary. Unsupported platforms never fall back to copying.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
#[cfg(target_os = "macos")]
use std::os::darwin::fs::MetadataExt as DarwinMetadataExt;
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
    },
    path::Path,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Fingerprint {
    pub dev: u64,
    pub ino: u64,
    pub len: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: (i64, i64),
    pub ctime: (i64, i64),
    pub atime: (i64, i64),
    pub birth: (i64, i64),
    pub flags: u32,
    pub digest: String,
    pub xattrs: BTreeMap<String, String>,
    pub acl: String,
}
impl Fingerprint {
    pub fn equivalent(&self, other: &Self) -> bool {
        self.len == other.len
            && self.mode == other.mode
            && self.uid == other.uid
            && self.gid == other.gid
            && self.mtime == other.mtime
            && self.birth == other.birth
            && self.flags == other.flags
            && self.digest == other.digest
            && self.preserved_xattrs().eq(other.preserved_xattrs())
            && self.acl == other.acl
    }
    pub fn identity(&self) -> (u64, u64) {
        (self.dev, self.ino)
    }
    pub fn unchanged(&self, other: &Self) -> bool {
        self.identity() == other.identity()
            && self.ctime == other.ctime
            && self.xattrs == other.xattrs
            && self.equivalent(other)
    }
    pub fn preserves(&self, other: &Self) -> bool {
        self.equivalent(other) && self.atime == other.atime
    }

    fn preserved_xattrs(&self) -> impl Iterator<Item = (&String, &String)> {
        // The kernel may assign new provenance to a clone; it cannot be restored by the process.
        // Keep it in the fingerprint so same-file mutation checks remain strict.
        self.xattrs
            .iter()
            .filter(|(name, _)| name.as_str() != "com.apple.provenance")
    }

    pub fn preservation_differences(&self, other: &Self) -> BTreeMap<String, serde_json::Value> {
        let mut differences = BTreeMap::new();
        macro_rules! fields {
            ($($field:ident),+ $(,)?) => {$(
                if self.$field != other.$field {
                    differences.insert(stringify!($field).into(), serde_json::json!({
                        "expected": self.$field, "observed": other.$field
                    }));
                }
            )+};
        }
        fields!(len, mode, uid, gid, mtime, atime, birth, flags, digest, acl);
        for name in self.xattrs.keys().chain(other.xattrs.keys()) {
            if name != "com.apple.provenance" && self.xattrs.get(name) != other.xattrs.get(name) {
                differences.insert(
                    format!("xattr {name}"),
                    serde_json::json!({
                        "expected": self.xattrs.get(name), "observed": other.xattrs.get(name)
                    }),
                );
            }
        }
        differences
    }
}
pub fn open_regular(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let m = file.metadata()?;
    if !m.is_file()
        || m.nlink() != 1
        || m.mode() & 0o7000 != 0
        || m.uid() != unsafe { libc::geteuid() }
        || flags(&m) != 0
    {
        return Err(Error::new(
            "E_UNSUPPORTED",
            format!(
                "unsupported file type, hard links, owner, set-id bits or flags: {}",
                path.display()
            ),
        ));
    }
    Ok(file)
}

pub fn preserve_group(path: &Path, gid: u32) -> Result<()> {
    use std::os::fd::AsRawFd;

    let file = open_regular(path)?;
    // APFS clones inherit the destination directory's group, even when owner copying is enabled.
    if file.metadata()?.gid() != gid
        && unsafe { libc::fchown(file.as_raw_fd(), libc::uid_t::MAX, gid) } != 0
    {
        return Err(Error::new(
            "E_IO",
            format!(
                "could not preserve clone group for {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ),
        )
        .with_data(serde_json::json!({"path": path, "field": "gid", "expected": gid})));
    }
    Ok(())
}

pub fn fingerprint(path: &Path) -> Result<Fingerprint> {
    let mut file = open_regular(path)?;
    let before = file.metadata()?;
    let mut hash = blake3::Hasher::new();
    let mut buffer = [0u8; 131072];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    let after = file.metadata()?;
    if before.ino() != after.ino()
        || before.len() != after.len()
        || (
            before.mtime(),
            before.mtime_nsec(),
            before.ctime(),
            before.ctime_nsec(),
        ) != (
            after.mtime(),
            after.mtime_nsec(),
            after.ctime(),
            after.ctime_nsec(),
        )
    {
        return Err(Error::new(
            "E_CHANGED",
            format!("file changed while reading: {}", path.display()),
        ));
    }
    let mut attrs = BTreeMap::new();
    for name in xattr::list(path)? {
        let key = name
            .into_string()
            .map_err(|_| Error::new("E_UNSUPPORTED", "non-UTF-8 extended attribute name"))?;
        if let Some(value) = xattr::get(path, &key)? {
            attrs.insert(key, blake3::hash(&value).to_hex().to_string());
        }
    }
    let m = fs::symlink_metadata(path)?;
    if m.ino() != before.ino()
        || m.dev() != before.dev()
        || m.ctime() != before.ctime()
        || m.ctime_nsec() != before.ctime_nsec()
    {
        return Err(Error::new(
            "E_CHANGED",
            "file identity or metadata changed while reading",
        ));
    }
    Ok(Fingerprint {
        dev: m.dev(),
        ino: m.ino(),
        len: m.len(),
        mode: m.mode(),
        uid: m.uid(),
        gid: m.gid(),
        mtime: (m.mtime(), m.mtime_nsec()),
        ctime: (m.ctime(), m.ctime_nsec()),
        atime: (m.atime(), m.atime_nsec()),
        birth: birth(&m),
        flags: flags(&m),
        digest: hash.finalize().to_hex().to_string(),
        xattrs: attrs,
        acl: acl(path)?,
    })
}

#[cfg(target_os = "macos")]
fn flags(m: &fs::Metadata) -> u32 {
    m.st_flags()
}
#[cfg(not(target_os = "macos"))]
fn flags(_: &fs::Metadata) -> u32 {
    0
}
#[cfg(target_os = "macos")]
fn birth(m: &fs::Metadata) -> (i64, i64) {
    (m.st_birthtime(), m.st_birthtime_nsec())
}
#[cfg(not(target_os = "macos"))]
fn birth(_: &fs::Metadata) -> (i64, i64) {
    (0, 0)
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use std::{ffi::CString, os::fd::AsRawFd};
    unsafe extern "C" {
        fn fclonefileat(
            src: libc::c_int,
            dir: libc::c_int,
            dst: *const libc::c_char,
            flags: u32,
        ) -> libc::c_int;
        fn fcopyfile(
            src: libc::c_int,
            dst: libc::c_int,
            state: *mut libc::c_void,
            flags: u32,
        ) -> libc::c_int;
        fn renamex_np(a: *const libc::c_char, b: *const libc::c_char, flags: u32) -> libc::c_int;
        fn acl_get_file(path: *const libc::c_char, kind: libc::c_int) -> *mut libc::c_void;
        fn acl_to_text(acl: *mut libc::c_void, len: *mut libc::ssize_t) -> *mut libc::c_char;
        fn acl_free(object: *mut libc::c_void) -> libc::c_int;
    }
    fn cpath(path: &Path) -> Result<CString> {
        CString::new(path.as_os_str().as_bytes())
            .map_err(|_| Error::new("E_ARGUMENT", "NUL in path"))
    }
    pub fn clone(src: &Path, dst: &Path) -> Result<()> {
        let file = open_regular(src)?;
        let dst = cpath(dst)?;
        // The descriptor pins the donor inode; CLONE_ACL preserves ACLs for later verification.
        if unsafe { fclonefileat(file.as_raw_fd(), libc::AT_FDCWD, dst.as_ptr(), 5) } != 0 {
            let error = std::io::Error::last_os_error();
            let code = match error.raw_os_error() {
                Some(libc::ENOTSUP | libc::EXDEV) => "E_CLONE",
                Some(libc::EEXIST) => "E_CHANGED",
                _ => "E_IO",
            };
            return Err(Error::new(code, error.to_string()));
        }
        Ok(())
    }
    pub fn metadata(src: &Path, dst: &Path) -> Result<()> {
        let source = open_regular(src)?;
        let dest = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dst)?;
        if unsafe {
            fcopyfile(
                source.as_raw_fd(),
                dest.as_raw_fd(),
                std::ptr::null_mut(),
                7,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        // copyfile preserves the older creation time, so conversion must restore the target's explicitly.
        let m = source.metadata()?;
        let mut attrs = AttrList {
            count: 5,
            reserved: 0,
            common: 0x00000200,
            volume: 0,
            directory: 0,
            file: 0,
            extended: 0,
        };
        let mut birth = libc::timespec {
            tv_sec: m.st_birthtime(),
            tv_nsec: m.st_birthtime_nsec(),
        };
        if unsafe {
            libc::fsetattrlist(
                dest.as_raw_fd(),
                (&mut attrs as *mut AttrList).cast(),
                (&mut birth as *mut libc::timespec).cast(),
                std::mem::size_of::<libc::timespec>(),
                0,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    pub fn swap(a: &Path, b: &Path) -> Result<()> {
        let a = cpath(a)?;
        let b = cpath(b)?;
        if unsafe { renamex_np(a.as_ptr(), b.as_ptr(), 2) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    pub fn acl(path: &Path) -> Result<String> {
        let c_path = cpath(path)?;
        let value = unsafe { acl_get_file(c_path.as_ptr(), 0x100) };
        if value.is_null() {
            let error = std::io::Error::last_os_error();
            // Darwin reports ENOENT for a present file with no extended ACL.
            if error.raw_os_error() == Some(libc::ENOENT) && fs::symlink_metadata(path).is_ok() {
                return Ok(String::new());
            }
            return Err(error.into());
        }
        let mut len = 0;
        let raw = unsafe { acl_to_text(value, &mut len) };
        if raw.is_null() {
            unsafe {
                acl_free(value);
            }
            return Err(std::io::Error::last_os_error().into());
        }
        let text = String::from_utf8_lossy(unsafe {
            std::slice::from_raw_parts(raw.cast::<u8>(), len as usize)
        })
        .to_string();
        unsafe {
            acl_free(raw.cast());
            acl_free(value);
        }
        Ok(text)
    }
    #[repr(C)]
    struct AttrList {
        count: u16,
        reserved: u16,
        common: u32,
        volume: u32,
        directory: u32,
        file: u32,
        extended: u32,
    }
    pub fn clone_id(path: &Path) -> Result<Option<u64>> {
        let path = cpath(path)?;
        let mut attrs = AttrList {
            count: 5,
            reserved: 0,
            common: 0,
            volume: 0,
            directory: 0,
            file: 0,
            extended: 0x100,
        };
        let mut buffer = [0u8; 16];
        let r = unsafe {
            libc::getattrlist(
                path.as_ptr(),
                (&mut attrs as *mut AttrList).cast(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                0x20,
            )
        };
        if r != 0 {
            return Ok(None);
        }
        let id = u64::from_ne_bytes(
            buffer[4..12]
                .try_into()
                .map_err(|_| Error::new("E_IO", "invalid clone ID"))?,
        );
        Ok((id != 0).then_some(id))
    }
    pub fn supported(path: &Path) -> Result<()> {
        let path = cpath(path)?;
        let mut value = std::mem::MaybeUninit::<libc::statfs>::uninit();
        if unsafe { libc::statfs(path.as_ptr(), value.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let value = unsafe { value.assume_init() };
        let name = unsafe { std::ffi::CStr::from_ptr(value.f_fstypename.as_ptr()) };
        if name.to_bytes() != b"apfs" {
            return Err(Error::new("E_UNSUPPORTED", "only macOS/APFS is supported"));
        }
        if value.f_flags & libc::MNT_IGNORE_OWNERSHIP as u32 != 0 {
            return Err(Error::new("E_UNSUPPORTED", "APFS volumes with ownership disabled cannot enforce workspace permission safeguards; enable ownership in macOS before using this volume"));
        }
        Ok(())
    }
}
#[cfg(target_os = "macos")]
pub use native::{acl, clone, clone_id, metadata, supported, swap};
#[cfg(not(target_os = "macos"))]
pub fn supported(_: &Path) -> Result<()> {
    Err(Error::new(
        "E_UNSUPPORTED",
        "native CoW requires macOS/APFS",
    ))
}
#[cfg(not(target_os = "macos"))]
pub fn clone(_: &Path, _: &Path) -> Result<()> {
    supported(Path::new("."))
}
#[cfg(not(target_os = "macos"))]
pub fn metadata(_: &Path, _: &Path) -> Result<()> {
    supported(Path::new("."))
}
#[cfg(not(target_os = "macos"))]
pub fn swap(_: &Path, _: &Path) -> Result<()> {
    supported(Path::new("."))
}
#[cfg(not(target_os = "macos"))]
pub fn clone_id(_: &Path) -> Result<Option<u64>> {
    Ok(None)
}
#[cfg(not(target_os = "macos"))]
pub fn acl(_: &Path) -> Result<String> {
    Ok(String::new())
}

pub fn probe(source_dir: &Path, destination_dir: &Path) -> Result<()> {
    supported(source_dir)?;
    supported(destination_dir)?;
    if fs::metadata(source_dir)?.dev() != fs::metadata(destination_dir)?.dev() {
        return Err(Error::new(
            "E_CLONE",
            "source and destination are on different volumes",
        ));
    }
    // The volume identity check lets the probe avoid creating anything in a user checkout.
    let mut source = tempfile::Builder::new()
        .prefix(".tool-probe-")
        .tempfile_in(destination_dir)?;
    source.write_all(b"copy-on-write capability probe")?;
    source.as_file().sync_all()?;
    let container = tempfile::Builder::new()
        .prefix(".tool-probe-")
        .tempdir_in(destination_dir)?;
    let dest = container.path().join("clone");
    clone(source.path(), &dest)?;
    if fs::read(&dest)? != b"copy-on-write capability probe" {
        return Err(Error::new("E_CLONE", "clone probe byte mismatch"));
    }
    fs::write(&dest, b"private write")?;
    if fs::read(source.path())? != b"copy-on-write capability probe" {
        return Err(Error::new(
            "E_CLONE",
            "clone probe failed independent-write check",
        ));
    }
    Ok(())
}
pub fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

pub fn validate_workspace_parent(path: &Path) -> Result<()> {
    let user = unsafe { libc::geteuid() };
    for directory in path.ancestors() {
        supported(directory)?;
        let metadata = fs::symlink_metadata(directory)?;
        if !metadata.is_dir() || ![0, user].contains(&metadata.uid()) {
            return Err(Error::new(
                "E_UNSUPPORTED",
                "destination ancestors must be directories owned by the current user or root",
            ));
        }
        if metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0 {
            return Err(Error::new("E_UNSUPPORTED", "destination ancestors cannot be group/world writable without sticky-directory protection"));
        }
        // Deny entries cannot grant rename rights; allow entries need a separate principal-aware policy.
        if acl(directory)?
            .lines()
            .any(|line| !line.is_empty() && !line.starts_with("!#acl") && !line.contains(":deny:"))
        {
            return Err(Error::new(
                "E_UNSUPPORTED",
                "destination ancestors with allow ACL entries are unsupported",
            ));
        }
    }
    Ok(())
}
