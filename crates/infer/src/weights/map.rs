//! Read-only memory mappings and the in-place reading of their bytes as tensors: the one module
//! of `weights` where ADR-0087 section 5 lifts the `unsafe` denial (the other is `kernels`).
//!
//! Everything here is a safe API over four kinds of `unsafe` operation: `mmap` of a file
//! (private, read-only), `mmap` of anonymous memory with `mprotect` to read-only after it is
//! filled, `munmap` on drop, and the view of a byte slice as a slice of [`Plain`] values. The
//! rest of the crate sees only `&[u8]` borrowed from a [`Mapping`] and `&[T]` from [`cast`].
//!
//! **The one precondition the type system cannot check:** a mapped weight file is not written
//! or truncated while it is mapped. Writing through another descriptor would change bytes behind
//! a shared borrow, and truncating it makes the next touch of a page past the new end raise
//! `SIGBUS`. The converter writes a new file beside the target and renames it into place, so a
//! replaced file is a new inode and the old mapping keeps the old bytes. The same holds for
//! every file-backed `mmap` in Rust (`memmap2` puts it on the caller as an `unsafe fn`); ADR-0087
//! section 2 chose the mapping for its memory profile, and weight files are written once.

use std::ffi::c_void;
use std::fmt;
use std::fs::File;
use std::io;
use std::ptr::{self, NonNull};

use rustix::mm::{self, MapFlags, MprotectFlags, ProtFlags};

/// A read-only mapping of `len > 0` bytes, unmapped on drop.
pub(super) struct Mapping {
    ptr: NonNull<c_void>,
    len: usize,
}

// SAFETY: the mapping is read-only memory owned by this value alone (no other handle to it
// exists in the process), so moving it to another thread or reading it from several threads
// at once races with no writer.
unsafe impl Send for Mapping {}
// SAFETY: as for `Send`; `&Mapping` gives only shared read access.
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Maps the whole of `file` (open for reading) private and read-only, at the length it has
    /// now.
    ///
    /// # Errors
    /// `InvalidData` for an empty file or one larger than the address space; the `mmap`
    /// failure otherwise (not a regular file, out of address space, ...).
    pub(super) fn of_file(file: &File) -> io::Result<Self> {
        let len = usize::try_from(file.metadata()?.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "file too large to map"))?;
        if len == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "empty file"));
        }
        // SAFETY: a null address without `MAP_FIXED` lets the kernel pick a free range, so no
        // existing memory is replaced. `len` is the file's current length and greater than 0,
        // so every byte of the mapping is backed by the file (or by the zero tail of its last
        // page); it is not grown later and read only through `bytes`. `PROT_READ` with
        // `MAP_PRIVATE` gives no write access to the file. The file's bytes stay unchanged
        // while mapped by the module's precondition (see the module docs).
        let ptr = unsafe {
            mm::mmap(
                ptr::null_mut(),
                len,
                ProtFlags::READ,
                MapFlags::PRIVATE,
                file,
                0,
            )
        }?;
        let ptr = NonNull::new(ptr).ok_or_else(|| io::Error::other("mmap returned null"))?;
        Ok(Self { ptr, len })
    }

    /// Copies `bytes` (not empty) into a new anonymous mapping and makes it read-only: page
    /// aligned like a file mapping, for weight files that do not come from a file.
    ///
    /// # Errors
    /// `InvalidInput` for empty `bytes`; the `mmap` or `mprotect` failure otherwise.
    pub(super) fn copy_of(bytes: &[u8]) -> io::Result<Self> {
        let len = bytes.len();
        if len == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty buffer"));
        }
        // SAFETY: a null address without `MAP_FIXED` lets the kernel pick a free range, so no
        // existing memory is replaced; `len` is greater than 0.
        let raw = unsafe {
            mm::mmap_anonymous(
                ptr::null_mut(),
                len,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::PRIVATE,
            )
        }?;
        let ptr = NonNull::new(raw).ok_or_else(|| io::Error::other("mmap returned null"))?;
        // From here on `Drop` unmaps it on every path, including the `mprotect` failure.
        let mapping = Self { ptr, len };
        // SAFETY: the destination is the `len` writable bytes just mapped, which nothing else
        // references, and cannot overlap `bytes`, which is borrowed memory that existed before.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), raw.cast::<u8>(), len) };
        // SAFETY: `raw` and `len` are exactly the mapping made above; no reference into it is
        // alive, so taking write access away invalidates nothing.
        unsafe { mm::mprotect(raw, len, MprotectFlags::READ) }?;
        Ok(mapping)
    }

    /// The mapped bytes.
    pub(super) fn bytes(&self) -> &[u8] {
        // SAFETY: `ptr` is the start of a live mapping of `len` readable bytes (unmapped only
        // in `drop`, which the borrow of `self` excludes); `len > 0` fits `isize` because the
        // kernel mapped it. No `&mut` to it exists: the anonymous mapping was written once
        // before `copy_of` returned and is read-only since, and the file mapping is read-only
        // with its file unchanged by the module's precondition.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr().cast::<u8>(), self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `ptr` and `len` are exactly one mapping made by `of_file` or `copy_of` and
        // not unmapped before; `&mut self` proves no borrow from `bytes` is alive. A failure
        // (only possible for arguments this type never holds) leaves the range mapped, which
        // leaks address space but is not unsound.
        let _ = unsafe { mm::munmap(self.ptr.as_ptr(), self.len) };
    }
}

impl fmt::Debug for Mapping {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mapping")
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

/// Element types a weight file holds in place.
///
/// # Safety
/// Every bit pattern of the type's size is a valid value and the type has no padding, so any
/// initialised bytes may be read as it. Implementing it needs `unsafe`, which the crate denies
/// outside this module and `kernels`.
pub(super) unsafe trait Plain: Copy {}
// SAFETY: one byte, every pattern a valid `i8`.
unsafe impl Plain for i8 {}
// SAFETY: one byte, every pattern a valid `u8`.
unsafe impl Plain for u8 {}
// SAFETY: four bytes, every pattern a valid `u32`, no padding.
unsafe impl Plain for u32 {}
// SAFETY: four bytes, every pattern a valid `f32` (NaNs included), no padding.
unsafe impl Plain for f32 {}
// SAFETY: eight bytes, every pattern a valid `f64` (NaNs included), no padding.
unsafe impl Plain for f64 {}

/// `bytes` viewed in place as values of `T` in the target's byte order, or `None` if `bytes`
/// does not start at an address aligned for `T` or is not a whole number of values.
pub(super) fn cast<T: Plain>(bytes: &[u8]) -> Option<&[T]> {
    // SAFETY: `T: Plain`: any bytes of its size are a valid `T` and it has no padding, so
    // reading initialised `u8`s as `T` is sound; `align_to` itself puts only aligned,
    // in-bounds elements into the middle slice, which keeps `bytes`' lifetime.
    let (head, values, tail) = unsafe { bytes.align_to::<T>() };
    (head.is_empty() && tail.is_empty()).then_some(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cast_checks_alignment_and_length() {
        let m = Mapping::copy_of(&[0, 0, 128, 63, 0, 0, 0, 64, 9]).unwrap();
        let b = m.bytes();
        assert_eq!(b.as_ptr().align_offset(4096), 0, "page aligned");
        assert_eq!(cast::<f32>(&b[..8]), Some(&[1.0f32, 2.0][..]));
        assert_eq!(cast::<u32>(&b[..4]), Some(&[0x3f80_0000u32][..]));
        assert_eq!(cast::<i8>(&b[8..]), Some(&[9i8][..]));
        assert_eq!(cast::<u8>(&b[8..]), Some(&[9u8][..]));
        assert_eq!(
            cast::<f64>(&b[..8]),
            Some(&[f64::from_bits(0x4000_0000_3f80_0000)][..])
        );
        assert!(cast::<f64>(&b[1..9]).is_none(), "misaligned for f64");
        assert!(cast::<f32>(&b[1..5]).is_none(), "misaligned start");
        assert!(cast::<f32>(&b[..6]).is_none(), "not whole values");
        assert_eq!(cast::<f32>(&b[..0]), Some(&[][..]));
    }

    #[test]
    fn empty_mappings_are_refused() {
        assert!(Mapping::copy_of(&[]).is_err());
    }
}
