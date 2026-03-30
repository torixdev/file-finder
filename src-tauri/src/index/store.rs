use super::entry::Entry;
use super::trigram::TrigramIndex;
use ahash::AHashMap as HashMap;
use arc_swap::ArcSwap;
use std::sync::Arc;

pub struct Index {
    pub entries: Vec<Entry>,
    pub arena: Vec<u8>,
    pub ext_map: HashMap<Box<[u8]>, Vec<u32>>,
    pub trigram: TrigramIndex,
}

impl Index {
    #[inline(always)]
    pub fn get_bytes(&self, off: u32, len: usize) -> &[u8] {
        &self.arena[off as usize..off as usize + len]
    }

    #[inline(always)]
    pub fn get_str(&self, off: u32, len: usize) -> &str {
        unsafe { std::str::from_utf8_unchecked(self.get_bytes(off, len)) }
    }

    #[inline(always)]
    pub fn entry_name(&self, e: &Entry) -> &str {
        self.get_str(e.name_off, e.name_len as usize)
    }

    #[inline(always)]
    pub fn entry_name_lower(&self, e: &Entry) -> &[u8] {
        self.get_bytes(e.name_lower_off, e.name_lower_len as usize)
    }

    #[inline(always)]
    pub fn entry_path(&self, e: &Entry) -> &str {
        self.get_str(e.path_off, e.path_len as usize)
    }
}

pub struct IndexStore {
    inner: ArcSwap<Option<Arc<Index>>>,
}

impl IndexStore {
    pub fn new() -> Self {
        Self {
            inner: ArcSwap::new(Arc::new(None)),
        }
    }

    pub fn store(&self, index: Index) {
        self.inner.store(Arc::new(Some(Arc::new(index))));
    }

    pub fn load(&self) -> Option<Arc<Index>> {
        let guard = self.inner.load();
        guard.as_ref().clone()
    }
}