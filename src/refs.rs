//! Windows block cloning via `FSCTL_DUPLICATE_EXTENTS_TO_FILE`.

use std::ffi::c_void;
use std::fs::{self, File};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, symlink_dir, symlink_file};
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr::{from_ref, null, null_mut};

use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_SPARSE_FILE, FILE_FLAG_OPEN_REPARSE_POINT,
    GetDiskFreeSpaceW, GetVolumePathNameW,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DUPLICATE_EXTENTS_DATA, FSCTL_DUPLICATE_EXTENTS_TO_FILE, FSCTL_SET_SPARSE,
};

const CLONE_LIMIT: u64 = 1 << 32;

pub(super) fn clone_into(src: &Path, dst_dir: &Path) -> io::Result<()> {
    let name = src
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "source has no name"))?;
    clone_entry(src, &dst_dir.join(name), cluster_size(dst_dir)?)
        .map_err(|e| io::Error::other(format!("block clone {}: {e}", src.display())))
}

fn cluster_size(dir: &Path) -> io::Result<u64> {
    const CAPACITY: u32 = 32_768;
    // Resolve junctions and mount points so we query the actual destination volume.
    let dir = fs::canonicalize(dir)?;
    let path: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
    let mut root = vec![0u16; usize::try_from(CAPACITY).expect("Windows uses at least 32 bits")];
    // SAFETY: path is NUL-terminated, and root is writable for CAPACITY code units.
    if unsafe { GetVolumePathNameW(path.as_ptr(), root.as_mut_ptr(), CAPACITY) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let (mut sectors, mut bytes) = (0, 0);
    // SAFETY: root is NUL-terminated by GetVolumePathNameW; the unused outputs may be null.
    if unsafe {
        GetDiskFreeSpaceW(
            root.as_ptr(),
            &raw mut sectors,
            &raw mut bytes,
            null_mut(),
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let size = u64::from(sectors) * u64::from(bytes);
    if !size.is_power_of_two() || size >= CLONE_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid cluster size",
        ));
    }
    Ok(size)
}

fn clone_entry(src: &Path, dst: &Path, cluster: u64) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    let kind = meta.file_type();
    if kind.is_symlink_dir() {
        return symlink_dir(fs::read_link(src)?, dst);
    }
    if kind.is_symlink_file() {
        return symlink_file(fs::read_link(src)?, dst);
    }
    if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported reparse point",
        ));
    }
    if kind.is_dir() {
        fs::create_dir(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            clone_entry(&entry.path(), &dst.join(entry.file_name()), cluster)?;
        }
        return Ok(());
    }
    clone_file(src, dst, cluster)
}

fn clone_file(src: &Path, dst: &Path, cluster: u64) -> io::Result<()> {
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
    let dst = File::create_new(dst)?;
    let len = meta.len();
    if len != 0 {
        if meta.file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE != 0 {
            block_clone_ioctl(&dst, None)?;
        }
        // Include the cluster containing EOF. The destination must span the whole range before
        // cloning; truncating back to the original size can make the last cluster private.
        let aligned = len.next_multiple_of(cluster);
        dst.set_len(aligned)?;
        duplicate_extents(&src, &dst, aligned, cluster)?;
        if len != aligned {
            dst.set_len(len)?;
        }
    }
    dst.set_modified(meta.modified()?)
}

fn duplicate_extents(src: &File, dst: &File, len: u64, cluster: u64) -> io::Result<()> {
    // Each request must be cluster-aligned and strictly smaller than 4 GiB.
    let max_chunk = (CLONE_LIMIT - 1) / cluster * cluster;
    let offset = |n| i64::try_from(n).map_err(io::Error::other);
    let mut start = 0;
    while start < len {
        let size = (len - start).min(max_chunk);
        let range = DUPLICATE_EXTENTS_DATA {
            FileHandle: src.as_raw_handle(),
            SourceFileOffset: offset(start)?,
            TargetFileOffset: offset(start)?,
            ByteCount: offset(size)?,
        };
        block_clone_ioctl(dst, Some(&range))?;
        start += size;
    }
    Ok(())
}

// Prepare a sparse destination or duplicate a range of blocks into it.
fn block_clone_ioctl(file: &File, range: Option<&DUPLICATE_EXTENTS_DATA>) -> io::Result<()> {
    let (code, input, size) = match range {
        Some(range) => (
            FSCTL_DUPLICATE_EXTENTS_TO_FILE,
            from_ref(range).cast::<c_void>(),
            u32::try_from(size_of::<DUPLICATE_EXTENTS_DATA>()).expect("fixed-size Win32 struct"),
        ),
        None => (FSCTL_SET_SPARSE, null(), 0),
    };
    let mut bytes = 0;
    // SAFETY: range, when supplied, and the files live through this synchronous call. The
    // range's caller keeps the source handle open. Neither code needs an output buffer.
    if unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            code,
            input,
            size,
            null_mut(),
            0,
            &raw mut bytes,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
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
    use std::io::{Read, Seek, SeekFrom, Write};

    #[test]
    fn clones_nested_files_and_isolates_writes() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        if !crate::can_clone(temp.path(), temp.path()) {
            return Ok(());
        }
        let src = temp.path().join("src");
        let dst = temp.path().join("dst");
        fs::create_dir_all(src.join("nested"))?;
        fs::create_dir(&dst)?;
        let cluster = usize::try_from(cluster_size(&dst)?).map_err(io::Error::other)?;
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
        let temp = tempfile::tempdir()?;
        if !crate::can_clone(temp.path(), temp.path()) {
            return Ok(());
        }
        let src = temp.path().join("sparse");
        let dst = temp.path().join("dst");
        fs::create_dir(&dst)?;
        let mut file = File::create_new(&src)?;
        block_clone_ioctl(&file, None)?;
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
}
