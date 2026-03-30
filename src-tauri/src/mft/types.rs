use winapi::shared::{
    minwindef::DWORD,
    ntdef::{LARGE_INTEGER, LONGLONG, ULONGLONG},
};

pub const FSCTL_ENUM_USN_DATA: DWORD = 0x000900b3;
pub const FSCTL_QUERY_USN_JOURNAL: DWORD = 0x000900f4;
pub const MFT_BUFFER_SIZE: usize = 32 * 1024 * 1024;
pub const ROOT_FILE_REF: u64 = 0x0005_0000_0000_0005;
pub const FILE_REF_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct MftEnumDataV0 {
    pub start_file_reference_number: ULONGLONG,
    pub low_usn: LONGLONG,
    pub high_usn: LONGLONG,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct UsnJournalDataV0 {
    pub usn_journal_id: ULONGLONG,
    pub first_usn: LONGLONG,
    pub next_usn: LONGLONG,
    pub lowest_valid_usn: LONGLONG,
    pub max_usn: LONGLONG,
    pub maximum_size: ULONGLONG,
    pub allocation_delta: ULONGLONG,
}

#[repr(C)]
pub struct UsnRecordV2 {
    pub record_length: DWORD,
    pub major_version: u16,
    pub minor_version: u16,
    pub file_reference_number: ULONGLONG,
    pub parent_file_reference_number: ULONGLONG,
    pub usn: LONGLONG,
    pub time_stamp: LARGE_INTEGER,
    pub reason: DWORD,
    pub source_info: DWORD,
    pub security_id: DWORD,
    pub file_attributes: DWORD,
    pub file_name_length: u16,
    pub file_name_offset: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MftRawEntry {
    pub file_ref: u64,
    pub parent_ref: u64,
    pub name_off: u32,
    pub name_len: u16,
    pub is_dir: bool,
    pub is_hidden: bool,
    pub modified: u64,
}