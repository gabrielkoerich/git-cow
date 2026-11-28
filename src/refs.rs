//! Windows block cloning via `FSCTL_DUPLICATE_EXTENTS_TO_FILE`.

mod win32;

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt, symlink_dir, symlink_file};
use std::path::Path;

use win32::{
    FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN,
    FILE_ATTRIBUTE_NOT_CONTENT_INDEXED, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_SPARSE_FILE, FILE_ATTRIBUTE_SYSTEM, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK,
    IntegrityInformation,
};

const CLONE_LIMIT: u64 = 1 << 32;
const BASIC_ATTRIBUTES: u32 = FILE_ATTRIBUTE_READONLY
    | FILE_ATTRIBUTE_HIDDEN
    | FILE_ATTRIBUTE_SYSTEM
    | FILE_ATTRIBUTE_ARCHIVE
    | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED;

pub(super) fn clone_into(src: &Path, dst_dir: &Path) -> io::Result<()> {
    let name = src
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "source has no name"))?;
    clone_entry(src, &dst_dir.join(name))
        .map_err(|e| io::Error::new(e.kind(), format!("block clone {}: {e}", src.display())))
}

fn cluster_size(info: &IntegrityInformation) -> io::Result<u64> {
    // Alignment is in filesystem clusters, not integrity checksum chunks.
    let size = u64::from(info.cluster_size);
    if !size.is_power_of_two() || size >= CLONE_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid cluster size",
        ));
    }
    Ok(size)
}

fn clone_entry(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return clone_reparse_point(src, dst, &meta);
    }
    if meta.is_dir() {
        fs::create_dir(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            clone_entry(&entry.path(), &dst.join(entry.file_name()))?;
        }
        return Ok(());
    }
    clone_file(src, dst)
}

fn reparse_options() -> OpenOptions {
    let mut options = File::options();
    options.custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    options
}

fn read_reparse_data(path: &Path) -> io::Result<Vec<u8>> {
    // GET_REPARSE_POINT only needs a handle, not permission to list the directory.
    win32::reparse_data(&reparse_options().access_mode(0).open(path)?)
}

fn reparse_tag(data: &[u8]) -> u32 {
    u32::from_le_bytes(*data.first_chunk().expect("validated reparse header"))
}

fn clone_reparse_point(src: &Path, dst: &Path, meta: &fs::Metadata) -> io::Result<()> {
    let data = read_reparse_data(src)?;
    match reparse_tag(&data) {
        IO_REPARSE_TAG_MOUNT_POINT if !win32::is_volume_mount(&data)? => {
            // A junction is not a directory symlink. Reuse its record without resolving its target.
            fs::create_dir(dst)?;
            let result = reparse_options()
                .write(true)
                .open(dst)
                .and_then(|file| win32::set_reparse_data(&file, &data));
            if result.is_err() {
                let _ = fs::remove_dir(dst);
            }
            result
        }
        IO_REPARSE_TAG_SYMLINK if meta.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0 => {
            symlink_dir(fs::read_link(src)?, dst)
        }
        IO_REPARSE_TAG_SYMLINK => symlink_file(fs::read_link(src)?, dst),
        _ => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported reparse point",
        )),
    }
}

fn clone_file(src: &Path, dst: &Path) -> io::Result<()> {
    let src = File::options()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(src)?;
    let meta = src.metadata()?;
    if !meta.is_file() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "not a regular file",
        ));
    }
    let info = win32::integrity(&src)?;
    let cluster = cluster_size(&info)?;
    let file = File::create_new(dst)?;
    let result = clone_file_data(&src, &file, &meta, &info, cluster);
    drop(file);
    if result.is_err() {
        let _ = fs::remove_file(dst);
    }
    result
}

fn clone_file_data(
    src: &File,
    dst: &File,
    meta: &fs::Metadata,
    info: &IntegrityInformation,
    cluster: u64,
) -> io::Result<()> {
    // The destination may have inherited a different integrity setting from its parent.
    // Set it while the destination is still empty, including for zero-length files.
    win32::set_integrity(dst, info.algorithm, info.flags)?;
    let attributes = meta.file_attributes();
    if attributes & FILE_ATTRIBUTE_SPARSE_FILE != 0 {
        win32::set_sparse(dst)?;
    }
    let len = meta.len();
    if len != 0 {
        // Include the cluster containing EOF. The destination must span the whole range before
        // cloning; truncating back to the original size can make the last cluster private.
        let aligned = len.next_multiple_of(cluster);
        dst.set_len(aligned)?;
        duplicate_extents(src, dst, aligned, cluster)?;
        if len != aligned {
            dst.set_len(len)?;
        }
    }
    dst.set_modified(meta.modified()?)?;
    win32::set_attributes(dst, attributes & BASIC_ATTRIBUTES)
}

fn duplicate_extents(src: &File, dst: &File, len: u64, cluster: u64) -> io::Result<()> {
    // Each request must be cluster-aligned and strictly smaller than 4 GiB.
    let max_chunk = (CLONE_LIMIT - 1) / cluster * cluster;
    let offset = |n| i64::try_from(n).map_err(io::Error::other);
    let mut start = 0;
    while start < len {
        let size = (len - start).min(max_chunk);
        win32::duplicate_extents(src, dst, offset(start)?, offset(size)?)?;
        start += size;
    }
    Ok(())
}

#[cfg(test)]
#[expect(
    clippy::panic_in_result_fn,
    reason = "tests propagate I/O errors and assert clone invariants"
)]
mod tests {
    use super::*;
    use crate::{can_clone, remove_any};
    use std::env;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::process::Command;
    use tempfile::{TempDir, tempdir};

    fn clone_scratch() -> io::Result<Option<TempDir>> {
        let temp = tempdir()?;
        let supported = can_clone(temp.path(), temp.path());
        if env::var("GIT_COW_EXPECT_CLONE").as_deref() == Ok("1") && !supported {
            return Err(io::Error::other(format!(
                "GIT_COW_EXPECT_CLONE=1 but block cloning is unavailable in {}",
                temp.path().display()
            )));
        }
        Ok(supported.then_some(temp))
    }

    #[test]
    fn aligns_to_clusters_not_checksum_chunks() -> io::Result<()> {
        let info = IntegrityInformation {
            checksum_chunk_size: 4096,
            cluster_size: 65_536,
            ..IntegrityInformation::default()
        };
        assert_eq!(cluster_size(&info)?, 65_536);
        Ok(())
    }

    #[test]
    fn clones_nested_files_and_isolates_writes() -> io::Result<()> {
        let Some(temp) = clone_scratch()? else {
            return Ok(());
        };
        let src = temp.path().join("src");
        let dst = temp.path().join("dst");
        fs::create_dir_all(src.join("nested"))?;
        fs::create_dir(&dst)?;
        let probe = File::create_new(temp.path().join("probe"))?;
        let info = win32::integrity(&probe)?;
        let cluster = usize::try_from(cluster_size(&info)?).map_err(io::Error::other)?;
        for len in [0, 1, cluster, cluster + 31] {
            fs::write(src.join("nested").join(len.to_string()), vec![7; len])?;
        }
        clone_into(&src, &dst)?;
        for len in [0, 1, cluster, cluster + 31] {
            let src = src.join("nested").join(len.to_string());
            let dst = dst.join("src/nested").join(len.to_string());
            assert_eq!(fs::read(&dst)?, vec![7; len]);
            assert_eq!(
                fs::metadata(&src)?.modified()?,
                fs::metadata(&dst)?.modified()?
            );
            File::options().write(true).open(dst)?.write_all(b"x")?;
            assert_eq!(fs::read(src)?, vec![7; len]);
        }
        Ok(())
    }

    #[test]
    fn clones_sparse_files_larger_than_one_request() -> io::Result<()> {
        let Some(temp) = clone_scratch()? else {
            return Ok(());
        };
        let src = temp.path().join("sparse");
        let dst = temp.path().join("dst");
        fs::create_dir(&dst)?;
        let mut file = File::create_new(&src)?;
        win32::set_sparse(&file)?;
        file.write_all(b"abc")?;
        file.seek(SeekFrom::Start(CLONE_LIMIT))?;
        file.write_all(b"xyz")?;
        drop(file);

        clone_into(&src, &dst)?;
        let mut cloned = File::open(dst.join("sparse"))?;
        let meta = cloned.metadata()?;
        assert_eq!(meta.len(), CLONE_LIMIT + 3);
        assert_ne!(meta.file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE, 0);
        let mut buf = [0; 3];
        for (offset, expected) in [
            (0, b"abc"),
            (CLONE_LIMIT / 2, b"\0\0\0"),
            (CLONE_LIMIT, b"xyz"),
        ] {
            cloned.seek(SeekFrom::Start(offset))?;
            cloned.read_exact(&mut buf)?;
            assert_eq!(&buf, expected);
        }
        Ok(())
    }

    #[test]
    fn preserves_integrity_when_parent_defaults_differ() -> io::Result<()> {
        const NONE: u16 = 0;
        const CRC64: u16 = 2;
        const ENFORCEMENT_OFF: u32 = 1;
        let Some(temp) = clone_scratch()? else {
            return Ok(());
        };
        for (source, inherited, flags) in [
            (NONE, CRC64, 0),
            (CRC64, NONE, 0),
            (CRC64, NONE, ENFORCEMENT_OFF),
        ] {
            let dst = temp.path().join(format!("dst-{inherited}-{flags}"));
            fs::create_dir(&dst)?;
            let dir = reparse_options().read(true).write(true).open(&dst)?;
            win32::set_integrity(&dir, inherited, 0).expect("set destination directory default");
            drop(dir);
            let check = File::create_new(dst.join("inherits"))?;
            assert_eq!(win32::integrity(&check)?.algorithm != 0, inherited != NONE);
            drop(check);
            for payload in [b"data".as_slice(), b"".as_slice()] {
                let name = format!("source-{source}-{flags}-{}", payload.len());
                let src = temp.path().join(&name);
                let mut f = File::create_new(&src)?;
                win32::set_integrity(&f, source, flags).expect("set source file integrity");
                if !payload.is_empty() {
                    f.write_all(payload)?;
                }
                drop(f);
                let expected = win32::integrity(&File::open(&src)?)?;
                assert_eq!(expected.algorithm != 0, source != NONE);
                assert_eq!(expected.flags & ENFORCEMENT_OFF, flags);
                clone_into(&src, &dst)?;
                let actual = win32::integrity(&File::open(dst.join(&name))?)?;
                assert_eq!(
                    (actual.algorithm, actual.flags),
                    (expected.algorithm, expected.flags)
                );
                assert_eq!(fs::read(dst.join(&name))?, payload);
            }
        }
        Ok(())
    }

    #[test]
    fn preserves_basic_attributes_and_allows_cleanup() -> io::Result<()> {
        let Some(temp) = clone_scratch()? else {
            return Ok(());
        };
        let dst = temp.path().join("dst");
        fs::create_dir(&dst)?;
        for attrs in [0, BASIC_ATTRIBUTES] {
            let name = format!("source-{attrs}");
            let src = temp.path().join(&name);
            let mut file = File::create_new(&src)?;
            file.write_all(b"contents")?;
            win32::set_attributes(&file, attrs)?;
            drop(file);
            assert_eq!(
                fs::metadata(&src)?.file_attributes() & BASIC_ATTRIBUTES,
                attrs
            );
            clone_into(&src, &dst)?;
            let cloned = dst.join(&name);
            assert_eq!(
                fs::metadata(&cloned)?.file_attributes() & BASIC_ATTRIBUTES,
                attrs
            );
            assert_eq!(fs::read(cloned)?, b"contents");
        }
        temp.close()
    }

    #[test]
    fn recreates_junction_as_junction_without_following_target() -> io::Result<()> {
        let temp = tempdir()?;
        let target = temp.path().join("target");
        let src = temp.path().join("junction");
        let dst = temp.path().join("dst");
        fs::create_dir(&target)?;
        fs::create_dir(&dst)?;
        // mklink is independent of our implementation and needs no symlink privilege for /J.
        let result = Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J", "junction", "target"])
            .current_dir(temp.path())
            .output()?;
        assert!(
            result.status.success(),
            "mklink failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let record = read_reparse_data(&src)?;
        assert_eq!(reparse_tag(&record), IO_REPARSE_TAG_MOUNT_POINT);
        // A dangling target proves the clone never traversed it.
        fs::remove_dir(&target)?;
        clone_into(&src, &dst)?;
        let cloned = dst.join("junction");
        let cloned_record = read_reparse_data(&cloned)?;
        assert_eq!(reparse_tag(&cloned_record), IO_REPARSE_TAG_MOUNT_POINT);
        assert_eq!(cloned_record, record);

        fs::create_dir(&target)?;
        fs::write(target.join("keep"), b"outside the cloned tree")?;
        remove_any(&dst);
        assert!(!dst.exists(), "cleanup must remove the cloned junction");
        assert_eq!(fs::read(target.join("keep"))?, b"outside the cloned tree");
        temp.close()
    }

    #[test]
    fn distinguishes_mounted_volumes_from_junctions() -> io::Result<()> {
        let root = r"\??\Volume{26a21bda-a627-11d7-9931-806e6f6e6963}\";
        for (name, mounted) in [(root.to_owned(), true), (format!("{root}dir"), false)] {
            let name: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
            let mut record = vec![0; 16];
            let len = u16::try_from(name.len()).expect("short substitute name");
            record[10..12].copy_from_slice(&len.to_le_bytes());
            record.extend(name);
            assert_eq!(win32::is_volume_mount(&record)?, mounted);
        }
        Ok(())
    }

    #[test]
    fn recreates_relative_dangling_symlinks() -> io::Result<()> {
        const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;
        let temp = tempdir()?;
        let dst = temp.path().join("dst");
        fs::create_dir(&dst)?;
        let target = Path::new("missing");
        for (name, directory) in [("file", false), ("dir", true)] {
            let src = temp.path().join(name);
            let created = if directory {
                symlink_dir(target, &src)
            } else {
                symlink_file(target, &src)
            };
            match created {
                Err(e) if e.raw_os_error() == Some(ERROR_PRIVILEGE_NOT_HELD) => return Ok(()),
                result => result?,
            }
            clone_into(&src, &dst)?;
            let cloned = dst.join(name);
            assert_eq!(
                reparse_tag(&read_reparse_data(&cloned)?),
                IO_REPARSE_TAG_SYMLINK
            );
            assert_eq!(fs::read_link(cloned)?, target);
        }
        temp.close()
    }
}
