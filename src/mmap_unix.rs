// Copyright (C) 2019 Alibaba Cloud Computing. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

//! A default Unix implementation of the GuestMemory trait by mmap()-ing guest's memory into
//! the current process.
//!
//! The main structs to access guest's memory are:
//! - [MmapRegion](struct.MmapRegion.html): mmap a continuous region of guest's memory into the
//! current process
//! - [GuestRegionMmap](struct.GuestRegionMmap.html): tracks a mapping of memory in the current
//! process and the corresponding base address. It relays guest memory access requests to the
//! underline [MmapRegion](struct.MmapRegion.html) object.
//! - [GuestMemoryMmap](struct.GuestMemoryMmap.html): provides methods to access a collection of
//! GuestRegionMmap objects.

use std::io;
use std::os::unix::io::AsRawFd;
use std::ptr::null_mut;
use std::result;

use libc;

use guest_memory::FileOffset;
use mmap::AsSlice;
use volatile_memory::{self, compute_offset, VolatileMemory, VolatileSlice};

/// Error conditions that may appear when creating a new `MmapRegion` object.
#[derive(Debug)]
pub enum Error {
    /// The specified file offset and length cause overflow when added.
    InvalidOffsetLength,
    /// The forbidden `MAP_FIXED` flag was specified.
    MapFixed,
    /// Mappings using the same fd overlap in terms of file offset and length.
    MappingOverlap,
    /// A mapping with offset + length > EOF was attempted.
    MappingPastEof,
    /// The `mmap` call returned an error.
    Mmap(io::Error),
}

pub type Result<T> = result::Result<T, Error>;

/// A backend driver to access guest's physical memory by mmapping guest's memory into the current
/// process.
/// For a combination of 32-bit hypervisor and 64-bit virtual machine, only partial of guest's
/// physical memory may be mapped into current process due to limited process virtual address
/// space size.
#[derive(Debug)]
pub struct MmapRegion {
    addr: *mut u8,
    size: usize,
    file_offset: Option<FileOffset>,
    prot: i32,
    flags: i32,
}

// Send and Sync aren't automatically inherited for the raw address pointer.
// Accessing that pointer is only done through the stateless interface which
// allows the object to be shared by multiple threads without a decrease in
// safety.
unsafe impl Send for MmapRegion {}
unsafe impl Sync for MmapRegion {}

impl MmapRegion {
    /// Creates an anonymous shared mapping of `size` bytes.
    ///
    /// # Arguments
    /// * `size` - Size of memory region in bytes.
    pub fn new(size: usize) -> Result<Self> {
        Self::build(
            None,
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANONYMOUS | libc::MAP_NORESERVE | libc::MAP_PRIVATE,
        )
    }

    /// Maps the `size` bytes starting at `offset` bytes of the given `fd`.
    ///
    /// # Arguments
    /// * `file_offset` - File object and offset to mmap from.
    /// * `size` - Size of memory region in bytes.
    pub fn from_file(file_offset: FileOffset, size: usize) -> Result<Self> {
        Self::build(
            Some(file_offset),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_NORESERVE | libc::MAP_SHARED,
        )
    }

    /// Creates a new mapping based on the provided arguments.
    pub fn build(
        file_offset: Option<FileOffset>,
        size: usize,
        prot: i32,
        flags: i32,
    ) -> Result<Self> {
        // Forbid MAP_FIXED, as it doesn't make sense in this context, and is pretty dangerous
        // in general.
        if flags & libc::MAP_FIXED != 0 {
            return Err(Error::MapFixed);
        }

        let (fd, offset) = if let Some(ref f_off) = file_offset {
            let file = f_off.file();
            let start = f_off.start();

            if let Some(end) = start.checked_add(size as u64) {
                if let Ok(metadata) = file.metadata() {
                    if metadata.len() < end {
                        return Err(Error::MappingPastEof);
                    }
                }
            } else {
                return Err(Error::InvalidOffsetLength);
            }

            (file.as_raw_fd(), start)
        } else {
            (-1, 0)
        };

        // This is safe because we're not allowing MAP_FIXED, and invalid parameters cannot break
        // Rust safety guarantees (things may change if we're mapping /dev/mem or some wacky file).
        let addr = unsafe { libc::mmap(null_mut(), size, prot, flags, fd, offset as libc::off_t) };

        if addr == libc::MAP_FAILED {
            return Err(Error::Mmap(io::Error::last_os_error()));
        }

        Ok(Self {
            addr: addr as *mut u8,
            size,
            file_offset,
            prot,
            flags,
        })
    }

    /// Returns a pointer to the beginning of the memory region.  Should only be
    /// used for passing this region to ioctls for setting guest memory.
    pub fn as_ptr(&self) -> *mut u8 {
        self.addr
    }

    /// Returns the size of this region.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Returns information regarding the offset into the file backing this region (if any).
    pub fn file_offset(&self) -> Option<&FileOffset> {
        self.file_offset.as_ref()
    }

    /// Returns the value of the `prot` parameter passed to `mmap` when mapping this region.
    pub fn prot(&self) -> i32 {
        self.prot
    }

    /// Returns the value of the `flags` parameter passed to `mmap` when mapping this region.
    pub fn flags(&self) -> i32 {
        self.flags
    }

    /// Returns true if `self` and `other` map the same file descriptor, and the `(offset, size)`
    /// pairs overlap. This is mostly a sanity check available for convenience, as different file
    /// descriptors can alias the same file.
    pub fn fd_overlaps(&self, other: &MmapRegion) -> bool {
        if let Some(f_off1) = self.file_offset() {
            if let Some(f_off2) = other.file_offset() {
                if f_off1.file().as_raw_fd() == f_off2.file().as_raw_fd() {
                    let s1 = f_off1.start();
                    let s2 = f_off2.start();
                    let l1 = self.len() as u64;
                    let l2 = other.len() as u64;

                    if s1 < s2 {
                        return s1 + l1 <= s2;
                    } else {
                        return s2 + l2 <= s1;
                    }
                }
            }
        }
        false
    }
}

impl AsSlice for MmapRegion {
    // Returns the region as a slice
    // used to do crap
    unsafe fn as_slice(&self) -> &[u8] {
        // This is safe because we mapped the area at addr ourselves, so this slice will not
        // overflow. However, it is possible to alias.
        std::slice::from_raw_parts(self.addr, self.size)
    }

    // safe because it's expected interior mutability
    #[allow(clippy::mut_from_ref)]
    unsafe fn as_mut_slice(&self) -> &mut [u8] {
        // This is safe because we mapped the area at addr ourselves, so this slice will not
        // overflow. However, it is possible to alias.
        std::slice::from_raw_parts_mut(self.addr, self.size)
    }
}

impl VolatileMemory for MmapRegion {
    fn len(&self) -> usize {
        self.size
    }

    fn get_slice(&self, offset: usize, count: usize) -> volatile_memory::Result<VolatileSlice> {
        let end = compute_offset(offset, count)?;
        if end > self.size {
            return Err(volatile_memory::Error::OutOfBounds { addr: end });
        }

        // Safe because we checked that offset + count was within our range and we only ever hand
        // out volatile accessors.
        Ok(unsafe { VolatileSlice::new((self.addr as usize + offset) as *mut _, count) })
    }
}

impl Drop for MmapRegion {
    fn drop(&mut self) {
        // This is safe because we mmap the area at addr ourselves, and nobody
        // else is holding a reference to it.
        unsafe {
            libc::munmap(self.addr as *mut libc::c_void, self.size);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Write;
    use std::slice;

    #[test]
    fn test_mmap_region_new() {
        assert!(MmapRegion::new(0).is_err());

        let size = 4096;

        let r = MmapRegion::new(4096).unwrap();
        assert_eq!(r.size(), size);
        assert!(r.file_offset().is_none());
        assert_eq!(r.prot(), libc::PROT_READ | libc::PROT_WRITE);
        assert_eq!(
            r.flags(),
            libc::MAP_ANONYMOUS | libc::MAP_NORESERVE | libc::MAP_PRIVATE
        );
    }

    #[test]
    fn test_mmap_region_from_file() {
        let mut f = tempfile::tempfile().unwrap();
        let offset: usize = 0;
        let buf1 = [1u8, 2, 3, 4, 5];

        f.write_all(buf1.as_ref()).unwrap();
        let r = MmapRegion::from_file(FileOffset::new(f, offset as u64), buf1.len()).unwrap();

        assert_eq!(r.size(), buf1.len() - offset);
        assert_eq!(r.file_offset().unwrap().start(), offset as u64);
        assert_eq!(r.prot(), libc::PROT_READ | libc::PROT_WRITE);
        assert_eq!(r.flags(), libc::MAP_NORESERVE | libc::MAP_SHARED);

        let buf2 = unsafe { slice::from_raw_parts(r.as_ptr(), buf1.len() - offset) };
        assert_eq!(&buf1[offset..], buf2);
    }
}