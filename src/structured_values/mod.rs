// Copyright 2021 Colin Finck <colin@reactos.org>
// SPDX-License-Identifier: GPL-2.0-or-later

mod attribute_list;
mod file_name;
mod index_allocation;
mod index_root;
mod object_id;
mod security_descriptor;
mod standard_information;
mod volume_information;
mod volume_name;

pub use attribute_list::*;
pub use file_name::*;
pub use index_allocation::*;
pub use index_root::*;
pub use object_id::*;
pub use security_descriptor::*;
pub use standard_information::*;
pub use volume_information::*;
pub use volume_name::*;

use crate::attribute::NtfsAttributeType;
use crate::error::Result;
use crate::value::slice::NtfsSliceValue;
use crate::value::NtfsValue;
use binread::io::{Read, Seek};
use bitflags::bitflags;

bitflags! {
    pub struct NtfsFileAttributeFlags: u32 {
        const READ_ONLY = 0x0001;
        const HIDDEN = 0x0002;
        const SYSTEM = 0x0004;
        const ARCHIVE = 0x0020;
        const DEVICE = 0x0040;
        const NORMAL = 0x0080;
        const TEMPORARY = 0x0100;
        const SPARSE_FILE = 0x0200;
        const REPARSE_POINT = 0x0400;
        const COMPRESSED = 0x0800;
        const OFFLINE = 0x1000;
        const NOT_CONTENT_INDEXED = 0x2000;
        const ENCRYPTED = 0x4000;
        const IS_DIRECTORY = 0x1000_0000;
    }
}

pub trait NtfsStructuredValue<'n, 'f>: Sized {
    const TY: NtfsAttributeType;

    /// Create a structured value from an arbitrary `NtfsValue`.
    fn from_value<T>(fs: &mut T, value: NtfsValue<'n, 'f>) -> Result<Self>
    where
        T: Read + Seek;
}

/// Create a structured value from an arbitrary data slice.
/// This is a fast path for the few structured values that are always in resident attributes.
pub trait NtfsStructuredValueFromResidentAttributeValue<'n, 'f>:
    NtfsStructuredValue<'n, 'f>
{
    fn from_resident_attribute_value(value: NtfsSliceValue<'f>) -> Result<Self>;
}
