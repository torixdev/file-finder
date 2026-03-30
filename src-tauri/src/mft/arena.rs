pub struct MftArena {
    buf: Vec<u8>,
}

impl MftArena {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
        }
    }

    #[inline]
    pub fn push(&mut self, s: &str) -> (u32, u16) {
        let off = self.buf.len() as u32;
        let len = s.len().min(u16::MAX as usize) as u16;
        self.buf.extend_from_slice(s.as_bytes());
        (off, len)
    }

    #[inline]
    pub fn get(&self, off: u32, len: u16) -> &str {
        unsafe {
            std::str::from_utf8_unchecked(&self.buf[off as usize..off as usize + len as usize])
        }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}