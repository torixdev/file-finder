use super::arena::MftArena;
use super::types::*;
use std::{
    ffi::OsStr,
    mem,
    os::windows::ffi::OsStrExt,
    ptr,
    slice,
    sync::atomic::{AtomicBool, Ordering},
};
use winapi::{
    shared::{
        minwindef::{DWORD, LPVOID},
        ntdef::HANDLE,
        winerror::ERROR_HANDLE_EOF,
    },
    um::{
        errhandlingapi::GetLastError,
        fileapi::{CreateFileW, OPEN_EXISTING},
        handleapi::{CloseHandle, INVALID_HANDLE_VALUE},
        ioapiset::DeviceIoControl,
        winbase::FILE_FLAG_BACKUP_SEMANTICS,
        winnt::{
            FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN, FILE_SHARE_READ, FILE_SHARE_WRITE,
            GENERIC_READ,
        },
    },
};

pub struct MftScanner {
    volume_handle: HANDLE,
}

unsafe impl Send for MftScanner {}
unsafe impl Sync for MftScanner {}

pub struct MftScanResult {
    pub entries: Vec<MftRawEntry>,
    pub arena: MftArena,
}

impl MftScanner {
    pub fn open(drive_letter: char) -> Result<Self, String> {
        unsafe {
            let volume_path = format!("\\\\.\\{}:", drive_letter);
            let wide: Vec<u16> = OsStr::new(&volume_path)
                .encode_wide()
                .chain(Some(0))
                .collect();

            let handle = CreateFileW(
                wide.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                ptr::null_mut(),
            );

            if handle == INVALID_HANDLE_VALUE {
                return Err(format!("Cannot open volume {}", drive_letter));
            }

            Ok(Self {
                volume_handle: handle,
            })
        }
    }

    fn query_usn_journal(&self) -> Result<UsnJournalDataV0, String> {
        unsafe {
            let mut journal_data: UsnJournalDataV0 = mem::zeroed();
            let mut bytes_returned: DWORD = 0;

            let result = DeviceIoControl(
                self.volume_handle,
                FSCTL_QUERY_USN_JOURNAL,
                ptr::null_mut(),
                0,
                &mut journal_data as *mut _ as LPVOID,
                mem::size_of::<UsnJournalDataV0>() as DWORD,
                &mut bytes_returned,
                ptr::null_mut(),
            );

            if result == 0 {
                return Err(format!("FSCTL_QUERY_USN_JOURNAL failed: {}", GetLastError()));
            }

            Ok(journal_data)
        }
    }

    pub fn enumerate(&self, cancel_flag: &AtomicBool) -> Result<MftScanResult, String> {
        let journal_data = self.query_usn_journal()?;

        let mut enum_data = MftEnumDataV0 {
            start_file_reference_number: 0,
            low_usn: 0,
            high_usn: journal_data.next_usn,
        };

        let mut buffer = vec![0u8; MFT_BUFFER_SIZE];
        let mut entries: Vec<MftRawEntry> = Vec::with_capacity(500_000);
        let mut arena = MftArena::with_capacity(500_000 * 24);
        let mut utf8_buf: Vec<u8> = Vec::with_capacity(512);

        loop {
            if cancel_flag.load(Ordering::Relaxed) {
                return Ok(MftScanResult { entries, arena });
            }

            let mut bytes_returned: DWORD = 0;
            let result = unsafe {
                DeviceIoControl(
                    self.volume_handle,
                    FSCTL_ENUM_USN_DATA,
                    &mut enum_data as *mut _ as LPVOID,
                    mem::size_of::<MftEnumDataV0>() as DWORD,
                    buffer.as_mut_ptr() as LPVOID,
                    MFT_BUFFER_SIZE as DWORD,
                    &mut bytes_returned,
                    ptr::null_mut(),
                )
            };

            if result == 0 {
                let err = unsafe { GetLastError() };
                if err == ERROR_HANDLE_EOF {
                    break;
                }
                return Err(format!("MFT read error: {}", err));
            }

            if bytes_returned < 8 {
                break;
            }

            Self::parse_buffer(
                &buffer,
                bytes_returned as usize,
                &mut enum_data,
                &mut entries,
                &mut arena,
                &mut utf8_buf,
            );
        }

        Ok(MftScanResult { entries, arena })
    }

    fn parse_buffer(
        buffer: &[u8],
        bytes_ret: usize,
        enum_data: &mut MftEnumDataV0,
        entries: &mut Vec<MftRawEntry>,
        arena: &mut MftArena,
        utf8_buf: &mut Vec<u8>,
    ) {
        let mut offset = 8usize;

        while offset + mem::size_of::<UsnRecordV2>() <= bytes_ret {
            let record = unsafe { &*(buffer.as_ptr().add(offset) as *const UsnRecordV2) };

            if record.record_length == 0 || record.record_length > 0x10000 {
                break;
            }

            let filename_offset = offset + record.file_name_offset as usize;
            let filename_wchars = (record.file_name_length / 2) as usize;

            if filename_offset + filename_wchars * 2 <= bytes_ret && filename_wchars > 0 {
                let filename_slice = unsafe {
                    let ptr = buffer.as_ptr().add(filename_offset) as *const u16;
                    slice::from_raw_parts(ptr, filename_wchars)
                };

                utf8_buf.clear();
                let valid = Self::utf16_to_utf8_fast(filename_slice, utf8_buf);

                if valid {
                    let name_str = unsafe { std::str::from_utf8_unchecked(utf8_buf) };

                    if Self::is_valid_filename(name_str) {
                        let is_dir =
                            (record.file_attributes & FILE_ATTRIBUTE_DIRECTORY) != 0;
                        let is_hidden =
                            (record.file_attributes & FILE_ATTRIBUTE_HIDDEN) != 0;

                        let modified = unsafe {
                            let ft = *record.time_stamp.QuadPart() as u64;
                            if ft > 116444736000000000 {
                                (ft - 116444736000000000) / 10000000
                            } else {
                                0
                            }
                        };

                        let (name_off, name_len) = arena.push(name_str);

                        entries.push(MftRawEntry {
                            file_ref: record.file_reference_number & FILE_REF_MASK,
                            parent_ref: record.parent_file_reference_number & FILE_REF_MASK,
                            name_off,
                            name_len,
                            is_dir,
                            is_hidden,
                            modified,
                        });
                    }
                }
            }

            enum_data.start_file_reference_number = record.file_reference_number;
            offset += record.record_length as usize;
        }
    }

    #[inline]
    fn utf16_to_utf8_fast(src: &[u16], dst: &mut Vec<u8>) -> bool {
        let mut all_ascii = true;
        for &c in src {
            if c > 127 {
                all_ascii = false;
                break;
            }
        }

        if all_ascii {
            dst.reserve(src.len());
            for &c in src {
                dst.push(c as u8);
            }
            return true;
        }

        for &c in src {
            if c < 0x80 {
                dst.push(c as u8);
            } else if c < 0x800 {
                dst.push(0xC0 | (c >> 6) as u8);
                dst.push(0x80 | (c & 0x3F) as u8);
            } else if (0xD800..=0xDBFF).contains(&c) {
                return false;
            } else {
                dst.push(0xE0 | (c >> 12) as u8);
                dst.push(0x80 | ((c >> 6) & 0x3F) as u8);
                dst.push(0x80 | (c & 0x3F) as u8);
            }
        }

        true
    }

    #[inline]
    fn is_valid_filename(name: &str) -> bool {
        !name.is_empty()
            && name != "."
            && name != ".."
            && !name.as_bytes().first().is_some_and(|&b| b == b'$')
    }
}

impl Drop for MftScanner {
    fn drop(&mut self) {
        unsafe {
            if self.volume_handle != INVALID_HANDLE_VALUE {
                CloseHandle(self.volume_handle);
            }
        }
    }
}