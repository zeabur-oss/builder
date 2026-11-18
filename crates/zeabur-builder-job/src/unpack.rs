//! Unpacks an archive source into the empty source folder.
//!
//! The rules ([`zeabur_builder_core::archive`]):
//! - An entry path that is absolute or holds `..` stops the unpack.
//! - A symbolic link must stay inside the folder. A hard link must name a regular file that the
//!   archive wrote before it.
//! - Device files and FIFOs are skipped.
//! - Nothing is written through a symbolic link: every parent of an entry must be a real directory.
//! - The decompressed stream is counted as it is read, so the unpack stops at the limit.
//!
//! Files get mode `0755` when the archive gives any execute bit, else `0644`. Owners and times
//! are not kept.

use std::collections::HashSet;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, ErrorKind, Read};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use flate2::read::MultiGzDecoder;
use tar::EntryType;
use zeabur_builder_core::archive::{entry_path, link_stays_inside};

/// A reader that fails once more than `limit` bytes have passed.
struct Capped<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        let n64 = u64::try_from(n).unwrap_or(u64::MAX);
        self.left = self.left.checked_sub(n64).ok_or_else(|| {
            io::Error::new(
                ErrorKind::FileTooLarge,
                "the archive unpacks past its limit",
            )
        })?;
        Ok(n)
    }
}

/// Unpacks the gzip tar `archive` into `root`, which must not exist yet.
pub(crate) fn unpack(archive: impl Read, root: &Path, limit: u64) -> Result<(), String> {
    DirBuilder::new()
        .mode(0o755)
        .create(root)
        .map_err(|e| format!("cannot create {}: {e}", root.display()))?;
    let stream = Capped {
        inner: MultiGzDecoder::new(archive),
        left: limit,
    };
    let mut tar = tar::Archive::new(stream);
    let mut files: HashSet<PathBuf> = HashSet::new();
    let entries = tar.entries().map_err(|e| read_error(&e))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| read_error(&e))?;
        let name = entry.path().map_err(|e| read_error(&e))?.into_owned();
        let path = entry_path(&name).map_err(|e| e.to_string())?;
        let kind = entry.header().entry_type();
        if path.as_os_str().is_empty() {
            if kind.is_dir() {
                continue;
            }
            return Err(format!(
                "the archive entry `{}` has no name",
                name.display()
            ));
        }
        let to = root.join(&path);
        #[expect(
            clippy::wildcard_enum_match_arm,
            reason = "EntryType is non-exhaustive"
        )]
        match kind {
            EntryType::Directory => {
                parents(root, &path)?;
                clear(&to, true)?;
                if !to.is_dir() {
                    DirBuilder::new()
                        .mode(0o755)
                        .create(&to)
                        .map_err(|e| write_error(&path, &e))?;
                }
                files.remove(&path);
            }
            EntryType::Regular | EntryType::Continuous => {
                parents(root, &path)?;
                clear(&to, false)?;
                let executable = entry.header().mode().is_ok_and(|m| m & 0o111 != 0);
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(if executable { 0o755 } else { 0o644 })
                    .open(&to)
                    .map_err(|e| write_error(&path, &e))?;
                io::copy(&mut entry, &mut file).map_err(|e| read_error(&e))?;
                files.insert(path);
            }
            EntryType::Symlink => {
                let target = entry
                    .link_name()
                    .map_err(|e| read_error(&e))?
                    .ok_or_else(|| format!("the link `{}` has no target", path.display()))?;
                if !link_stays_inside(&path, &target) {
                    return Err(format!(
                        "the link `{}` points outside the source folder",
                        path.display()
                    ));
                }
                parents(root, &path)?;
                clear(&to, false)?;
                std::os::unix::fs::symlink(&target, &to).map_err(|e| write_error(&path, &e))?;
                files.remove(&path);
            }
            EntryType::Link => {
                let target = entry
                    .link_name()
                    .map_err(|e| read_error(&e))?
                    .ok_or_else(|| format!("the link `{}` has no target", path.display()))?;
                let target = entry_path(&target).map_err(|e| e.to_string())?;
                let from = root.join(&target);
                if !files.contains(&target)
                    || !fs::symlink_metadata(&from).is_ok_and(|m| m.is_file())
                {
                    return Err(format!(
                        "the hard link `{}` does not name a file of the archive",
                        path.display()
                    ));
                }
                parents(root, &path)?;
                clear(&to, false)?;
                fs::hard_link(&from, &to).map_err(|e| write_error(&path, &e))?;
                files.insert(path);
            }
            // Device files, FIFOs and other kinds have no place in a build context.
            _ => {}
        }
    }
    Ok(())
}

fn read_error(e: &io::Error) -> String {
    format!("cannot read the archive: {e}")
}

fn write_error(path: &Path, e: &io::Error) -> String {
    format!("cannot write `{}`: {e}", path.display())
}

/// Creates the parents of `path` below `root`, and refuses a parent that is not a real directory.
fn parents(root: &Path, path: &Path) -> Result<(), String> {
    let mut at = root.to_path_buf();
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    for part in parent.components() {
        at.push(part);
        match fs::symlink_metadata(&at) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(format!(
                    "`{}` is below `{}`, which is not a directory",
                    path.display(),
                    part.as_os_str().display()
                ));
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                DirBuilder::new()
                    .mode(0o755)
                    .create(&at)
                    .map_err(|e| write_error(path, &e))?;
            }
            Err(e) => return Err(write_error(path, &e)),
        }
    }
    Ok(())
}

/// Removes a file or link that a later entry replaces. A directory stays for a directory entry and
/// stops the unpack for any other.
fn clear(to: &Path, directory: bool) -> Result<(), String> {
    match fs::symlink_metadata(to) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(write_error(to, &e)),
        Ok(meta) if meta.is_dir() => {
            if directory {
                Ok(())
            } else {
                Err(format!("`{}` would replace a directory", to.display()))
            }
        }
        Ok(_) => fs::remove_file(to).map_err(|e| write_error(to, &e)),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::*;

    /// One raw tar entry. The name is written as bytes, so it may hold what `tar::Builder` refuses.
    enum Entry<'a> {
        File(&'a str, &'a [u8]),
        Dir(&'a str),
        Symlink(&'a str, &'a str),
        Hardlink(&'a str, &'a str),
        Fifo(&'a str),
    }

    fn header(name: &str, kind: EntryType, size: u64, link: &str) -> tar::Header {
        let mut header = tar::Header::new_ustar();
        let bytes = header.as_old_mut();
        bytes.name.fill(0);
        for (to, from) in bytes.name.iter_mut().zip(name.bytes()) {
            *to = from;
        }
        for (to, from) in bytes.linkname.iter_mut().zip(link.bytes()) {
            *to = from;
        }
        header.set_entry_type(kind);
        header.set_size(size);
        header.set_mode(0o644);
        header.set_cksum();
        header
    }

    fn archive(entries: &[Entry<'_>]) -> Vec<u8> {
        let mut tar = Vec::new();
        for entry in entries {
            let (header, data): (tar::Header, &[u8]) = match *entry {
                Entry::File(name, data) => (
                    header(name, EntryType::Regular, data.len() as u64, ""),
                    data,
                ),
                Entry::Dir(name) => (header(name, EntryType::Directory, 0, ""), &[]),
                Entry::Symlink(name, to) => (header(name, EntryType::Symlink, 0, to), &[]),
                Entry::Hardlink(name, to) => (header(name, EntryType::Link, 0, to), &[]),
                Entry::Fifo(name) => (header(name, EntryType::Fifo, 0, ""), &[]),
            };
            tar.extend_from_slice(header.as_bytes());
            tar.extend_from_slice(data);
            tar.resize(tar.len().div_ceil(512) * 512, 0);
        }
        tar.resize(tar.len() + 1024, 0);
        let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
        gz.write_all(&tar).unwrap();
        gz.finish().unwrap()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zeabur-builder-unpack-{}-{name}",
            std::process::id()
        ));
        drop(fs::remove_dir_all(&dir));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn unpacks_files_directories_and_links_inside_the_folder() {
        let dir = scratch("ok");
        let root = dir.join("src");
        let bytes = archive(&[
            Entry::Dir("./"),
            Entry::File("./package.json", b"{}"),
            Entry::Dir("src/"),
            Entry::File("src/index.js", b"console.log(1)"),
            Entry::Symlink("src/main.js", "index.js"),
            Entry::Symlink("src/pkg.json", "../package.json"),
            Entry::Hardlink("copy.json", "package.json"),
            Entry::Fifo("pipe"),
        ]);
        unpack(bytes.as_slice(), &root, 1 << 20).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("src/main.js")).unwrap(),
            "console.log(1)"
        );
        assert_eq!(fs::read_to_string(root.join("src/pkg.json")).unwrap(), "{}");
        assert_eq!(fs::read_to_string(root.join("copy.json")).unwrap(), "{}");
        assert!(fs::symlink_metadata(root.join("pipe")).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_entries_that_reach_outside() {
        let cases: [(&str, Vec<Entry<'_>>); 7] = [
            ("absolute path", vec![Entry::File("/tmp/x", b"x")]),
            ("parent path", vec![Entry::File("a/../../x", b"x")]),
            ("absolute link", vec![Entry::Symlink("l", "/etc")]),
            ("climbing link", vec![Entry::Symlink("a/l", "../../x")]),
            (
                "write through a link",
                vec![Entry::Symlink("l", "."), Entry::File("l/x", b"x")],
            ),
            ("hard link outside", vec![Entry::Hardlink("h", "../x")]),
            (
                "hard link to a link",
                vec![Entry::Symlink("l", "a"), Entry::Hardlink("h", "l")],
            ),
        ];
        for (name, entries) in cases {
            let dir = scratch("outside");
            let root = dir.join("src");
            let result = unpack(archive(&entries).as_slice(), &root, 1 << 20);
            assert!(result.is_err(), "{name}");
            let mut left = Vec::new();
            for entry in fs::read_dir(&dir).unwrap() {
                left.push(entry.unwrap().file_name());
            }
            assert_eq!(left, ["src"], "{name} wrote next to the folder");
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn a_later_entry_cannot_replace_a_directory_or_a_linked_file() {
        let cases: [(&str, Vec<Entry<'_>>); 3] = [
            (
                "link over a directory",
                vec![Entry::File("a/f", b"x"), Entry::Symlink("a", ".")],
            ),
            (
                "file over a directory",
                vec![Entry::File("a/f", b"x"), Entry::File("a", b"x")],
            ),
            (
                "hard link to a file a link replaced",
                vec![
                    Entry::File("f", b"x"),
                    Entry::Symlink("f", "g"),
                    Entry::Hardlink("h", "f"),
                ],
            ),
        ];
        for (name, entries) in cases {
            let dir = scratch("replace");
            let root = dir.join("src");
            assert!(
                unpack(archive(&entries).as_slice(), &root, 1 << 20).is_err(),
                "{name}"
            );
            assert!(
                !fs::symlink_metadata(root.join("h")).is_ok_and(|m| m.is_file()),
                "{name}"
            );
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn stops_at_the_limit_while_it_unpacks() {
        let dir = scratch("limit");
        let root = dir.join("src");
        let big = vec![b'a'; 64 << 10];
        let bytes = archive(&[Entry::File("first", b"1"), Entry::File("big", &big)]);
        let result = unpack(bytes.as_slice(), &root, 16 << 10);
        assert!(result.is_err());
        assert!(fs::metadata(root.join("first")).is_ok());
        assert!(fs::metadata(root.join("big")).is_ok_and(|m| m.len() < 16 << 10));
        fs::remove_dir_all(&dir).unwrap();
    }
}
