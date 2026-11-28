//! The Win32 ABI used by the `ReFS` backend (see `winioctl.h` and `winbase.h`).

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::ptr::{from_mut, from_ref, null, null_mut};

pub(super) const FILE_ATTRIBUTE_READONLY: u32 = 0x0001;
pub(super) const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0002;
pub(super) const FILE_ATTRIBUTE_SYSTEM: u32 = 0x0004;
pub(super) const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0010;
pub(super) const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x0020;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x0080;
pub(super) const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x0200;
pub(super) const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
pub(super) const FILE_ATTRIBUTE_NOT_CONTENT_INDEXED: u32 = 0x2000;
pub(super) const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
pub(super) const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
pub(super) const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xa000_0003;
pub(super) const IO_REPARSE_TAG_SYMLINK: u32 = 0xa000_000c;

// These FSCTLs use METHOD_BUFFERED. The integrity and duplicate-extents codes have access
// requirements encoded in the control code, so do not substitute FILE_ANY_ACCESS for them.
const FSCTL_GET_INTEGRITY_INFORMATION: u32 = 0x0009_027c;
const FSCTL_SET_INTEGRITY_INFORMATION: u32 = 0x0009_c280;
const FSCTL_GET_REPARSE_POINT: u32 = 0x0009_00a8;
const FSCTL_SET_REPARSE_POINT: u32 = 0x0009_00a4;
const FSCTL_SET_SPARSE: u32 = 0x0009_00c4;
const FSCTL_DUPLICATE_EXTENTS_TO_FILE: u32 = 0x0009_8344;
const FSCTL_INTEGRITY_FLAG_CHECKSUM_ENFORCEMENT_OFF: u32 = 1;
const FILE_BASIC_INFO_CLASS: i32 = 0; // FILE_INFO_BY_HANDLE_CLASS::FileBasicInfo
const MAXIMUM_REPARSE_DATA_BUFFER_SIZE: usize = 16 * 1024;

#[repr(C)]
#[derive(Default)]
pub(super) struct IntegrityInformation {
    pub(super) algorithm: u16,
    pub(super) reserved: u16,
    pub(super) flags: u32,
    pub(super) checksum_chunk_size: u32,
    pub(super) cluster_size: u32,
}

#[repr(C)]
struct SetIntegrityInformation {
    algorithm: u16,
    reserved: u16,
    flags: u32,
}

#[repr(C, align(8))]
struct DuplicateExtentsData {
    source: RawHandle,
    #[cfg(target_pointer_width = "32")]
    padding: u32,
    source_offset: i64,
    target_offset: i64,
    byte_count: i64,
}

#[repr(C, align(8))]
struct FileBasicInfo {
    creation_time: i64,
    last_access_time: i64,
    last_write_time: i64,
    change_time: i64,
    attributes: u32,
    padding: u32,
}

const _: () = {
    assert!(
        size_of::<IntegrityInformation>() == 16,
        "integrity query ABI"
    );
    assert!(
        size_of::<SetIntegrityInformation>() == 8,
        "integrity setter ABI"
    );
    assert!(
        size_of::<DuplicateExtentsData>() == 32,
        "extent cloning ABI"
    );
    assert!(size_of::<FileBasicInfo>() == 40, "basic file info ABI");
};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn DeviceIoControl(
        file: RawHandle,
        code: u32,
        input: *const c_void,
        input_size: u32,
        output: *mut c_void,
        output_size: u32,
        bytes_returned: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn SetFileInformationByHandle(
        file: RawHandle,
        class: i32,
        info: *const c_void,
        size: u32,
    ) -> i32;
}

fn win32_size(size: usize) -> u32 {
    u32::try_from(size).expect("Win32 structures and reparse records fit in a DWORD")
}

// SAFETY: the non-null buffers must be valid for the indicated byte counts and match `code`.
// Both they and any embedded handles must remain live through the synchronous call.
unsafe fn control(
    file: &File,
    code: u32,
    (input, input_size): (*const c_void, u32),
    (output, output_size): (*mut c_void, u32),
) -> io::Result<usize> {
    let mut bytes = 0;
    // SAFETY: the caller supplies valid buffers, and bytes is writable for the synchronous call.
    if unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            code,
            input,
            input_size,
            output,
            output_size,
            &raw mut bytes,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(usize::try_from(bytes).expect("Windows uses at least 32 bits"))
}

fn input<T>(value: &T) -> (*const c_void, u32) {
    (from_ref(value).cast(), win32_size(size_of::<T>()))
}

pub(super) fn integrity(file: &File) -> io::Result<IntegrityInformation> {
    let mut info = IntegrityInformation::default();
    // SAFETY: GET takes no input and writes the fixed-size integer-only integrity structure.
    let bytes = unsafe {
        control(
            file,
            FSCTL_GET_INTEGRITY_INFORMATION,
            (null(), 0),
            (
                from_mut(&mut info).cast(),
                win32_size(size_of::<IntegrityInformation>()),
            ),
        )?
    };
    if bytes != size_of::<IntegrityInformation>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid integrity response",
        ));
    }
    Ok(info)
}

pub(super) fn set_integrity(file: &File, algorithm: u16, flags: u32) -> io::Result<()> {
    let info = SetIntegrityInformation {
        algorithm,
        reserved: 0,
        flags: flags & FSCTL_INTEGRITY_FLAG_CHECKSUM_ENFORCEMENT_OFF,
    };
    // SAFETY: SET reads the integrity settings structure and does not return output.
    unsafe {
        control(
            file,
            FSCTL_SET_INTEGRITY_INFORMATION,
            input(&info),
            (null_mut(), 0),
        )?
    };
    Ok(())
}

pub(super) fn set_sparse(file: &File) -> io::Result<()> {
    // SAFETY: SET_SPARSE with no input marks the file as sparse; it produces no output.
    unsafe { control(file, FSCTL_SET_SPARSE, (null(), 0), (null_mut(), 0))? };
    Ok(())
}

pub(super) fn duplicate_extents(src: &File, dst: &File, offset: i64, len: i64) -> io::Result<()> {
    let range = DuplicateExtentsData {
        source: src.as_raw_handle(),
        #[cfg(target_pointer_width = "32")]
        padding: 0,
        source_offset: offset,
        target_offset: offset,
        byte_count: len,
    };
    // SAFETY: both handles and the input structure stay live through the call; no output.
    unsafe {
        control(
            dst,
            FSCTL_DUPLICATE_EXTENTS_TO_FILE,
            input(&range),
            (null_mut(), 0),
        )?
    };
    Ok(())
}

pub(super) fn reparse_data(file: &File) -> io::Result<Vec<u8>> {
    let mut data = vec![0; MAXIMUM_REPARSE_DATA_BUFFER_SIZE];
    // SAFETY: GET_REPARSE_POINT takes no input and gets the documented maximum output buffer.
    let bytes = unsafe {
        control(
            file,
            FSCTL_GET_REPARSE_POINT,
            (null(), 0),
            (data.as_mut_ptr().cast(), win32_size(data.len())),
        )?
    };
    if !(8..=data.len()).contains(&bytes) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid reparse data",
        ));
    }
    data.truncate(bytes);
    Ok(data)
}

// Junctions and mounted volumes share IO_REPARSE_TAG_MOUNT_POINT. The substitute name of
// a mounted volume is the volume GUID path itself, not a path to a directory on the volume.
pub(super) fn is_volume_mount(data: &[u8]) -> io::Result<bool> {
    const PREFIX: &str = r"\??\Volume{";
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid mount point record");
    let &[lo_offset, hi_offset, lo_len, hi_len] = data
        .get(8..12)
        .and_then(<[u8]>::first_chunk)
        .ok_or_else(invalid)?;
    let offset = usize::from(u16::from_le_bytes([lo_offset, hi_offset]));
    let len = usize::from(u16::from_le_bytes([lo_len, hi_len]));
    // The offsets are relative to the path buffer just after the 16-byte fixed header.
    let start = 16 + offset;
    let name = data.get(start..start + len).ok_or_else(invalid)?;
    if offset % 2 != 0 || len % 2 != 0 {
        return Err(invalid());
    }
    let (pairs, _) = name.as_chunks();
    let units: Vec<_> = pairs.iter().copied().map(u16::from_le_bytes).collect();
    let name = String::from_utf16_lossy(&units);
    let Some((prefix, rest)) = name.split_at_checked(PREFIX.len()) else {
        return Ok(false);
    };
    Ok(prefix.eq_ignore_ascii_case(PREFIX)
        && rest
            .trim_end_matches('\\')
            .strip_suffix('}')
            .is_some_and(|guid| guid.len() == 36 && !guid.contains('\\')))
}

pub(super) fn set_reparse_data(file: &File, data: &[u8]) -> io::Result<()> {
    // SAFETY: the byte buffer stays live, and this buffered IOCTL validates the supplied record.
    unsafe {
        control(
            file,
            FSCTL_SET_REPARSE_POINT,
            (data.as_ptr().cast(), win32_size(data.len())),
            (null_mut(), 0),
        )?
    };
    Ok(())
}

pub(super) fn set_attributes(file: &File, attributes: u32) -> io::Result<()> {
    let basic = FileBasicInfo {
        // Zero leaves timestamps unchanged. NORMAL resets settable attributes when none is set.
        creation_time: 0,
        last_access_time: 0,
        last_write_time: 0,
        change_time: 0,
        attributes: if attributes == 0 {
            FILE_ATTRIBUTE_NORMAL
        } else {
            attributes
        },
        padding: 0,
    };
    let (info, size) = input(&basic);
    // SAFETY: info points to a FILE_BASIC_INFO of the supplied size for this synchronous call.
    if unsafe {
        SetFileInformationByHandle(file.as_raw_handle(), FILE_BASIC_INFO_CLASS, info, size)
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
