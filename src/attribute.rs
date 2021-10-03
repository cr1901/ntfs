// Copyright 2021 Colin Finck <colin@reactos.org>
// SPDX-License-Identifier: GPL-2.0-or-later

use crate::error::{NtfsError, Result};
use crate::file::NtfsFile;
use crate::string::NtfsString;
use crate::structured_values::{
    NtfsAttributeList, NtfsAttributeListEntries, NtfsStructuredValue,
    NtfsStructuredValueFromResidentAttributeValue,
};
use crate::types::Vcn;
use crate::value::attribute_list_non_resident_attribute::NtfsAttributeListNonResidentAttributeValue;
use crate::value::non_resident_attribute::NtfsNonResidentAttributeValue;
use crate::value::slice::NtfsSliceValue;
use crate::value::NtfsValue;
use binread::io::{Read, Seek};
use bitflags::bitflags;
use byteorder::{ByteOrder, LittleEndian};
use core::iter::FusedIterator;
use core::mem;
use core::ops::Range;
use enumn::N;
use memoffset::offset_of;
use strum_macros::Display;

/// On-disk structure of the generic header of an NTFS attribute.
#[repr(C, packed)]
struct NtfsAttributeHeader {
    /// Type of the attribute, known types are in [`NtfsAttributeType`].
    ty: u32,
    /// Length of the resident part of this attribute, in bytes.
    length: u32,
    /// 0 if this attribute has a resident value, 1 if this attribute has a non-resident value.
    is_non_resident: u8,
    /// Length of the name, in UTF-16 code points (every code point is 2 bytes).
    name_length: u8,
    /// Offset to the beginning of the name, in bytes from the beginning of this header.
    name_offset: u16,
    /// Flags of the attribute, known flags are in [`NtfsAttributeFlags`].
    flags: u16,
    /// Identifier of this attribute that is unique within the [`NtfsFile`].
    instance: u16,
}

bitflags! {
    pub struct NtfsAttributeFlags: u16 {
        /// The attribute value is compressed.
        const COMPRESSED = 0x0001;
        /// The attribute value is encrypted.
        const ENCRYPTED = 0x4000;
        /// The attribute value is stored sparsely.
        const SPARSE = 0x8000;
    }
}

/// On-disk structure of the extra header of an NTFS attribute that has a resident value.
#[repr(C, packed)]
struct NtfsResidentAttributeHeader {
    attribute_header: NtfsAttributeHeader,
    /// Length of the value, in bytes.
    value_length: u32,
    /// Offset to the beginning of the value, in bytes from the beginning of the [`NtfsAttributeHeader`].
    value_offset: u16,
    /// 1 if this attribute (with resident value) is referenced in an index.
    indexed_flag: u8,
}

/// On-disk structure of the extra header of an NTFS attribute that has a non-resident value.
#[repr(C, packed)]
struct NtfsNonResidentAttributeHeader {
    attribute_header: NtfsAttributeHeader,
    /// Lower boundary of Virtual Cluster Numbers (VCNs) referenced by this attribute.
    /// This becomes relevant when file data is split over multiple attributes.
    /// Otherwise, it's zero.
    lowest_vcn: Vcn,
    /// Upper boundary of Virtual Cluster Numbers (VCNs) referenced by this attribute.
    /// This becomes relevant when file data is split over multiple attributes.
    /// Otherwise, it's zero (or even -1 for zero-length files according to NTFS-3G).
    highest_vcn: Vcn,
    /// Offset to the beginning of the value data runs.
    data_runs_offset: u16,
    /// Binary exponent denoting the number of clusters in a compression unit.
    /// A typical value is 4, meaning that 2^4 = 16 clusters are part of a compression unit.
    /// A value of zero means no compression (but that should better be determined via
    /// [`NtfsAttributeFlags`]).
    compression_unit_exponent: u8,
    reserved: [u8; 5],
    /// Allocated space for the attribute value, in bytes. This is always a multiple of the cluster size.
    /// For compressed files, this is always a multiple of the compression unit size.
    allocated_size: u64,
    /// Size of the attribute value, in bytes.
    /// This can be larger than `allocated_size` if the value is compressed or stored sparsely.
    data_size: u64,
    /// Size of the initialized part of the attribute value, in bytes.
    /// This is usually the same as `data_size`.
    initialized_size: u64,
}

#[derive(Clone, Copy, Debug, Display, Eq, N, PartialEq)]
#[repr(u32)]
pub enum NtfsAttributeType {
    StandardInformation = 0x10,
    AttributeList = 0x20,
    FileName = 0x30,
    ObjectId = 0x40,
    SecurityDescriptor = 0x50,
    VolumeName = 0x60,
    VolumeInformation = 0x70,
    Data = 0x80,
    IndexRoot = 0x90,
    IndexAllocation = 0xA0,
    Bitmap = 0xB0,
    ReparsePoint = 0xC0,
    EAInformation = 0xD0,
    EA = 0xE0,
    PropertySet = 0xF0,
    LoggedUtilityStream = 0x100,
    End = 0xFFFF_FFFF,
}

#[derive(Clone, Debug)]
pub struct NtfsAttribute<'n, 'f> {
    file: &'f NtfsFile<'n>,
    offset: usize,
    /// Has a value if this attribute's value may be split over multiple attributes.
    /// The connected attributes can be iterated using the encapsulated iterator.
    list_entries: Option<&'f NtfsAttributeListEntries<'n, 'f>>,
}

impl<'n, 'f> NtfsAttribute<'n, 'f> {
    pub(crate) fn new(
        file: &'f NtfsFile<'n>,
        offset: usize,
        list_entries: Option<&'f NtfsAttributeListEntries<'n, 'f>>,
    ) -> Self {
        Self {
            file,
            offset,
            list_entries,
        }
    }

    /// Returns the length of this NTFS attribute, in bytes.
    ///
    /// This denotes the length of the attribute structure on disk.
    /// Apart from various headers, this structure also includes the name and,
    /// for resident attributes, the actual value.
    pub fn attribute_length(&self) -> u32 {
        let start = self.offset + offset_of!(NtfsAttributeHeader, length);
        LittleEndian::read_u32(&self.file.record_data()[start..])
    }

    /// Returns flags set for this attribute as specified by [`NtfsAttributeFlags`].
    pub fn flags(&self) -> NtfsAttributeFlags {
        let start = self.offset + offset_of!(NtfsAttributeHeader, flags);
        NtfsAttributeFlags::from_bits_truncate(LittleEndian::read_u16(
            &self.file.record_data()[start..],
        ))
    }

    /// Returns the identifier of this attribute that is unique within the [`NtfsFile`].
    pub fn instance(&self) -> u16 {
        let start = self.offset + offset_of!(NtfsAttributeHeader, instance);
        LittleEndian::read_u16(&self.file.record_data()[start..])
    }

    /// Returns `true` if this is a resident attribute, i.e. one where its value
    /// is part of the attribute structure.
    pub fn is_resident(&self) -> bool {
        let start = self.offset + offset_of!(NtfsAttributeHeader, is_non_resident);
        let is_non_resident = self.file.record_data()[start];
        is_non_resident == 0
    }

    /// Gets the name of this NTFS attribute (if any) and returns it wrapped in an [`NtfsString`].
    ///
    /// Note that most NTFS attributes have no name and are distinguished by their types.
    /// Use [`NtfsAttribute::ty`] to get the attribute type.
    pub fn name(&self) -> Result<NtfsString<'f>> {
        if self.name_offset() == 0 || self.name_length() == 0 {
            return Ok(NtfsString(&[]));
        }

        self.validate_name_sizes()?;

        let start = self.offset + self.name_offset() as usize;
        let end = start + self.name_length();
        let string = NtfsString(&self.file.record_data()[start..end]);

        Ok(string)
    }

    fn name_offset(&self) -> u16 {
        let start = self.offset + offset_of!(NtfsAttributeHeader, name_offset);
        LittleEndian::read_u16(&self.file.record_data()[start..])
    }

    /// Returns the length of the name of this NTFS attribute, in bytes.
    ///
    /// An attribute name has a maximum length of 255 UTF-16 code points (510 bytes).
    /// It is always part of the attribute itself and hence also of the length
    /// returned by [`NtfsAttribute::attribute_length`].
    pub fn name_length(&self) -> usize {
        let start = self.offset + offset_of!(NtfsAttributeHeader, name_length);
        let name_length_in_characters = self.file.record_data()[start];
        name_length_in_characters as usize * mem::size_of::<u16>()
    }

    pub(crate) fn non_resident_value(&self) -> Result<NtfsNonResidentAttributeValue<'n, 'f>> {
        let (data, position) = self.non_resident_value_data_and_position();

        NtfsNonResidentAttributeValue::new(
            self.file.ntfs(),
            data,
            position,
            self.non_resident_value_data_size(),
        )
    }

    pub(crate) fn non_resident_value_data_and_position(&self) -> (&'f [u8], u64) {
        debug_assert!(!self.is_resident());
        let start = self.offset + self.non_resident_value_data_runs_offset() as usize;
        let end = start + self.attribute_length() as usize;
        let data = &self.file.record_data()[start..end];
        let position = self.file.position() + start as u64;

        (data, position)
    }

    fn non_resident_value_data_size(&self) -> u64 {
        debug_assert!(!self.is_resident());
        let start = self.offset + offset_of!(NtfsNonResidentAttributeHeader, data_size);
        LittleEndian::read_u64(&self.file.record_data()[start..])
    }

    fn non_resident_value_data_runs_offset(&self) -> u16 {
        debug_assert!(!self.is_resident());
        let start = self.offset + offset_of!(NtfsNonResidentAttributeHeader, data_runs_offset);
        LittleEndian::read_u16(&self.file.record_data()[start..])
    }

    pub(crate) fn offset(&self) -> usize {
        self.offset
    }

    /// Returns the absolute position of this NTFS attribute within the filesystem, in bytes.
    pub fn position(&self) -> u64 {
        self.file.position() + self.offset as u64
    }

    pub fn resident_structured_value<S>(&self) -> Result<S>
    where
        S: NtfsStructuredValueFromResidentAttributeValue<'n, 'f>,
    {
        let ty = self.ty()?;
        if ty != S::TY {
            return Err(NtfsError::AttributeOfDifferentType {
                position: self.position(),
                expected: S::TY,
                actual: ty,
            });
        }

        if !self.is_resident() {
            return Err(NtfsError::UnexpectedNonResidentAttribute {
                position: self.position(),
            });
        }

        let resident_value = self.resident_value()?;
        S::from_resident_attribute_value(resident_value)
    }

    pub(crate) fn resident_value(&self) -> Result<NtfsSliceValue<'f>> {
        debug_assert!(self.is_resident());
        self.validate_resident_value_sizes()?;

        let start = self.offset + self.resident_value_offset() as usize;
        let end = start + self.resident_value_length() as usize;
        let data = &self.file.record_data()[start..end];

        Ok(NtfsSliceValue::new(data, self.position()))
    }

    fn resident_value_length(&self) -> u32 {
        debug_assert!(self.is_resident());
        let start = self.offset + offset_of!(NtfsResidentAttributeHeader, value_length);
        LittleEndian::read_u32(&self.file.record_data()[start..])
    }

    fn resident_value_offset(&self) -> u16 {
        debug_assert!(self.is_resident());
        let start = self.offset + offset_of!(NtfsResidentAttributeHeader, value_offset);
        LittleEndian::read_u16(&self.file.record_data()[start..])
    }

    pub fn structured_value<T, S>(&self, fs: &mut T) -> Result<S>
    where
        T: Read + Seek,
        S: NtfsStructuredValue<'n, 'f>,
    {
        let ty = self.ty()?;
        if ty != S::TY {
            return Err(NtfsError::AttributeOfDifferentType {
                position: self.position(),
                expected: S::TY,
                actual: ty,
            });
        }

        S::from_value(fs, self.value()?)
    }

    /// Returns the type of this NTFS attribute, or [`NtfsError::UnsupportedAttributeType`]
    /// if it's an unknown type.
    pub fn ty(&self) -> Result<NtfsAttributeType> {
        let start = self.offset + offset_of!(NtfsAttributeHeader, ty);
        let ty = LittleEndian::read_u32(&self.file.record_data()[start..]);

        NtfsAttributeType::n(ty).ok_or(NtfsError::UnsupportedAttributeType {
            position: self.position(),
            actual: ty,
        })
    }

    fn validate_name_sizes(&self) -> Result<()> {
        let start = self.name_offset();
        if start as u32 >= self.attribute_length() {
            return Err(NtfsError::InvalidAttributeNameOffset {
                position: self.position(),
                expected: start,
                actual: self.attribute_length(),
            });
        }

        let end = start as usize + self.name_length();
        if end > self.attribute_length() as usize {
            return Err(NtfsError::InvalidAttributeNameLength {
                position: self.position(),
                expected: end,
                actual: self.attribute_length(),
            });
        }

        Ok(())
    }

    fn validate_resident_value_sizes(&self) -> Result<()> {
        debug_assert!(self.is_resident());

        let start = self.resident_value_offset();
        if start as u32 >= self.attribute_length() {
            return Err(NtfsError::InvalidResidentAttributeValueOffset {
                position: self.position(),
                expected: start,
                actual: self.attribute_length(),
            });
        }

        let end = start as u32 + self.resident_value_length();
        if end > self.attribute_length() {
            return Err(NtfsError::InvalidResidentAttributeValueLength {
                position: self.position(),
                expected: end,
                actual: self.attribute_length(),
            });
        }

        Ok(())
    }

    /// Returns an [`NtfsAttributeValue`] structure to read the value of this NTFS attribute.
    pub fn value(&self) -> Result<NtfsValue<'n, 'f>> {
        if let Some(list_entries) = self.list_entries {
            // The first attribute reports the entire data size for all connected attributes
            // (remaining ones are set to zero).
            // Fortunately, we are the first attribute :)
            let data_size = self.non_resident_value_data_size();

            let value = NtfsAttributeListNonResidentAttributeValue::new(
                self.file.ntfs(),
                list_entries.clone(),
                self.instance(),
                self.ty()?,
                data_size,
            );
            Ok(NtfsValue::AttributeListNonResidentAttribute(value))
        } else if self.is_resident() {
            let value = self.resident_value()?;
            Ok(NtfsValue::Slice(value))
        } else {
            let value = self.non_resident_value()?;
            Ok(NtfsValue::NonResidentAttribute(value))
        }
    }

    /// Returns the length of the value of this NTFS attribute, in bytes.
    pub fn value_length(&self) -> u64 {
        if self.is_resident() {
            self.resident_value_length() as u64
        } else {
            self.non_resident_value_data_size()
        }
    }
}

pub struct NtfsAttributes<'n, 'f> {
    raw_iter: NtfsAttributesRaw<'n, 'f>,
    list_entries: Option<NtfsAttributeListEntries<'n, 'f>>,
    list_skip_info: Option<(u16, NtfsAttributeType)>,
}

impl<'n, 'f> NtfsAttributes<'n, 'f> {
    pub(crate) fn new(file: &'f NtfsFile<'n>) -> Self {
        Self {
            raw_iter: NtfsAttributesRaw::new(file),
            list_entries: None,
            list_skip_info: None,
        }
    }

    pub fn next<T>(&mut self, fs: &mut T) -> Option<Result<NtfsAttributeItem<'n, 'f>>>
    where
        T: Read + Seek,
    {
        loop {
            if let Some(attribute_list_entries) = &mut self.list_entries {
                loop {
                    // If the next AttributeList entry turns out to be a non-resident attribute, that attribute's
                    // value may be split over multiple (adjacent) attributes.
                    // To view this value as a single one, we need an `AttributeListConnectedEntries` iterator
                    // and that iterator needs `NtfsAttributeListEntries` where the next call to `next` yields
                    // the first connected attribute.
                    // Therefore, we need to clone `attribute_list_entries` before every call.
                    let attribute_list_entries_clone = attribute_list_entries.clone();

                    let entry = match attribute_list_entries.next(fs) {
                        Some(Ok(entry)) => entry,
                        Some(Err(e)) => return Some(Err(e)),
                        None => break,
                    };
                    let entry_instance = entry.instance();
                    let entry_record_number = entry.base_file_reference().file_record_number();
                    let entry_ty = iter_try!(entry.ty());

                    // Ignore all AttributeList entries that just repeat attributes of the raw iterator.
                    if entry_record_number == self.raw_iter.file.file_record_number() {
                        continue;
                    }

                    // Ignore all AttributeList entries that are connected attributes of a previous one.
                    if let Some((skip_instance, skip_ty)) = self.list_skip_info {
                        if entry_instance == skip_instance && entry_ty == skip_ty {
                            continue;
                        }
                    }

                    // We found an attribute that we want to return.
                    self.list_skip_info = None;

                    let ntfs = self.raw_iter.file.ntfs();
                    let entry_file = iter_try!(entry.to_file(ntfs, fs));
                    let entry_attribute = iter_try!(entry.to_attribute(&entry_file));
                    let attribute_offset = entry_attribute.offset();

                    let mut list_entries = None;
                    if !entry_attribute.is_resident() {
                        list_entries = Some(attribute_list_entries_clone);
                        self.list_skip_info = Some((entry_instance, entry_ty));
                    }

                    let item = NtfsAttributeItem {
                        attribute_file: self.raw_iter.file,
                        attribute_value_file: Some(entry_file),
                        attribute_offset,
                        list_entries,
                    };
                    return Some(Ok(item));
                }
            }

            let attribute = self.raw_iter.next()?;
            if let Ok(NtfsAttributeType::AttributeList) = attribute.ty() {
                let attribute_list =
                    iter_try!(attribute.structured_value::<T, NtfsAttributeList>(fs));
                self.list_entries = Some(attribute_list.iter());
            } else {
                let item = NtfsAttributeItem {
                    attribute_file: self.raw_iter.file,
                    attribute_value_file: None,
                    attribute_offset: attribute.offset(),
                    list_entries: None,
                };
                return Some(Ok(item));
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct NtfsAttributeItem<'n, 'f> {
    attribute_file: &'f NtfsFile<'n>,
    attribute_value_file: Option<NtfsFile<'n>>,
    attribute_offset: usize,
    list_entries: Option<NtfsAttributeListEntries<'n, 'f>>,
}

impl<'n, 'f> NtfsAttributeItem<'n, 'f> {
    pub fn to_attribute<'i>(&'i self) -> NtfsAttribute<'n, 'i> {
        if let Some(file) = &self.attribute_value_file {
            NtfsAttribute::new(file, self.attribute_offset, self.list_entries.as_ref())
        } else {
            NtfsAttribute::new(
                self.attribute_file,
                self.attribute_offset,
                self.list_entries.as_ref(),
            )
        }
    }
}

pub struct NtfsAttributesRaw<'n, 'f> {
    file: &'f NtfsFile<'n>,
    items_range: Range<usize>,
}

impl<'n, 'f> NtfsAttributesRaw<'n, 'f> {
    pub(crate) fn new(file: &'f NtfsFile<'n>) -> Self {
        let start = file.first_attribute_offset() as usize;
        let end = file.used_size() as usize;
        let items_range = start..end;

        Self { file, items_range }
    }
}

impl<'n, 'f> Iterator for NtfsAttributesRaw<'n, 'f> {
    type Item = NtfsAttribute<'n, 'f>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.items_range.is_empty() {
            return None;
        }

        // This may be an entire attribute or just the 4-byte end marker.
        // Check if this marks the end of the attribute list.
        let ty = LittleEndian::read_u32(&self.file.record_data()[self.items_range.start..]);
        if ty == NtfsAttributeType::End as u32 {
            return None;
        }

        // It's a real attribute.
        let attribute = NtfsAttribute::new(self.file, self.items_range.start, None);
        self.items_range.start += attribute.attribute_length() as usize;

        Some(attribute)
    }
}

impl<'n, 'f> FusedIterator for NtfsAttributesRaw<'n, 'f> {}
