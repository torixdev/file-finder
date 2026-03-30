#[repr(C)]
#[derive(Clone, Copy)]
pub struct Entry {
    pub name_off: u32,
    pub name_len: u16,
    pub name_lower_off: u32,
    pub name_lower_len: u16,
    pub path_off: u32,
    pub path_len: u32,
    pub size: u64,
    pub modified: u64,
    pub flags: u16,
}

impl Entry {
    #[inline(always)]
    pub fn is_dir(&self) -> bool {
        (self.flags & 0x01) != 0
    }

    #[inline(always)]
    pub fn is_hidden(&self) -> bool {
        (self.flags & 0x02) != 0
    }
}