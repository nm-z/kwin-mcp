//! Explicit host-to-session refresh through the mounted filesystem, never by
//! modifying overlay upper layers behind the kernel's back.

use nix::fcntl::{OFlag, OpenHow, RenameFlags, ResolveFlag, openat2, renameat2};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub fn import_host_file(
    root: &Path,
    source: &Path,
    destination: &Path,
    overwrite: bool,
) -> Result<u64, String> {
    // Opening a FIFO must not block before we can reject non-regular files.
    let mut input = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(source)
        .map_err(|error| format!("read {}: {error}", source.display()))?;
    if !input
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err(format!("{} is not a regular file", source.display()));
    }
    let input_path = std::path::PathBuf::from(format!("/proc/self/fd/{}", input.as_raw_fd()));
    let name = destination
        .file_name()
        .ok_or_else(|| "session_path must name a file".to_owned())?;
    let parent = destination
        .parent()
        .ok_or_else(|| "session_path has no parent".to_owned())?;
    let root = std::fs::File::open(root).map_err(|error| format!("session root: {error}"))?;
    // A plain /proc/<pid>/root/path lookup follows absolute symlinks back into
    // the host namespace. Resolve them (and ..) inside the sandbox instead.
    // Pin the parent descriptor so renames of ancestors cannot redirect writes.
    let directory = openat2(
        &root,
        parent,
        OpenHow::new()
            .flags(OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC)
            .resolve(ResolveFlag::RESOLVE_IN_ROOT | ResolveFlag::RESOLVE_NO_MAGICLINKS),
    )
    .map_err(|error| format!("open session directory {}: {error}", parent.display()))?;
    let directory_path =
        std::path::PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
    let target = directory_path.join(name);
    if !overwrite && std::fs::symlink_metadata(&target).is_ok() {
        return Err(format!(
            "{} already exists in the session; pass overwrite=true to replace it",
            destination.display()
        ));
    }
    let temporary_name = format!(".kwin-mcp-import-{:016x}", rand::random::<u64>());
    let temporary = directory_path.join(&temporary_name);
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|error| format!("create temporary session file: {error}"))?;
    let outcome = (|| {
        let bytes = std::io::copy(&mut input, &mut output)
            .map_err(|error| format!("copy into session: {error}"))?;
        // Verify the opened host inode, even if its pathname is replaced while
        // copying. Reopening the pathname could also encounter a new FIFO.
        if !super::same_contents(&input_path, &temporary)
            .map_err(|error| format!("verify session copy: {error}"))?
        {
            return Err(
                "host file changed while importing; retry after the write finishes".to_owned(),
            );
        }
        // NOREPLACE also protects a destination created after the check above.
        // Rename replaces a final symlink itself, never its referent.
        renameat2(
            &directory,
            temporary_name.as_str(),
            &directory,
            name,
            if overwrite {
                RenameFlags::empty()
            } else {
                RenameFlags::RENAME_NOREPLACE
            },
        )
        .map_err(|error| format!("replace session file {}: {error}", destination.display()))?;
        Ok(bytes)
    })();
    if outcome.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    outcome
}
