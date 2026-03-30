use super::entry::Entry;
use super::trigram::TrigramIndex;
use crate::index::store::Index;
use ahash::AHashMap as HashMap;

pub struct IndexBuilder {
    entries: Vec<Entry>,
    arena: Vec<u8>,
    extensions: Vec<(Box<[u8]>, u32)>,
    trigram: TrigramIndex,
    lower_buf: Vec<u8>,
}

impl IndexBuilder {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            entries: Vec::with_capacity(cap),
            arena: Vec::with_capacity(cap * 120),
            extensions: Vec::with_capacity(cap / 2),
            trigram: TrigramIndex::new(),
            lower_buf: Vec::with_capacity(256),
        }
    }

    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    #[inline]
    fn intern(&mut self, s: &[u8]) -> u32 {
        let off = self.arena.len() as u32;
        self.arena.extend_from_slice(s);
        off
    }

    #[inline]
    fn ascii_lower_into_buf(&mut self, name: &str) {
        self.lower_buf.clear();
        let bytes = name.as_bytes();
        let is_ascii = bytes.iter().all(|&b| b < 128);

        if is_ascii {
            self.lower_buf.reserve(bytes.len());
            for &b in bytes {
                self.lower_buf.push(b.to_ascii_lowercase());
            }
        } else {
            let lower = name.to_lowercase();
            self.lower_buf.extend_from_slice(lower.as_bytes());
        }
    }

    pub fn add(
        &mut self,
        name: &str,
        path: &str,
        is_dir: bool,
        is_hidden: bool,
        size: u64,
        modified: u64,
    ) {
        self.add_inner(name, path.as_bytes(), is_dir, is_hidden, size, modified);
    }

    pub fn add_from_bytes(
        &mut self,
        name: &str,
        path_bytes: &[u8],
        is_dir: bool,
        is_hidden: bool,
        size: u64,
        modified: u64,
    ) {
        self.add_inner(name, path_bytes, is_dir, is_hidden, size, modified);
    }

    #[inline]
    fn add_inner(
        &mut self,
        name: &str,
        path_bytes: &[u8],
        is_dir: bool,
        is_hidden: bool,
        size: u64,
        modified: u64,
    ) {
        let id = self.entries.len() as u32;

        let name_off = self.intern(name.as_bytes());
        let name_len = name.len().min(u16::MAX as usize) as u16;

        self.ascii_lower_into_buf(name);

        let lower_clone = self.lower_buf.clone();
        let name_lower_off = self.intern(&lower_clone);
        let name_lower_len = lower_clone.len().min(u16::MAX as usize) as u16;

        self.trigram.insert(id, &lower_clone);

        let path_off = self.intern(path_bytes);
        let path_len = path_bytes.len() as u32;

        let mut flags = 0u16;
        if is_dir {
            flags |= 0x01;
        }
        if is_hidden {
            flags |= 0x02;
        }

        self.entries.push(Entry {
            name_off,
            name_len,
            name_lower_off,
            name_lower_len,
            path_off,
            path_len,
            size,
            modified,
            flags,
        });

        if !is_dir && !lower_clone.is_empty() {
            if let Some(dot_pos) = lower_clone.iter().rposition(|&b| b == b'.') {
                let ext = &lower_clone[dot_pos + 1..];
                if !ext.is_empty() && ext.len() <= 12 {
                    self.extensions.push((ext.to_vec().into_boxed_slice(), id));
                }
            }
        }
    }

    pub fn merge(&mut self, other: IndexBuilder) {
        let base_id = self.entries.len() as u32;
        let arena_base = self.arena.len() as u32;

        self.arena.extend_from_slice(&other.arena);

        for mut entry in other.entries {
            entry.name_off += arena_base;
            entry.name_lower_off += arena_base;
            entry.path_off += arena_base;
            self.entries.push(entry);
        }

        for (ext, id) in other.extensions {
            self.extensions.push((ext, id + base_id));
        }

        for (tri_hash, ids) in other.trigram.take_posting() {
            let shifted: Vec<u32> = ids.into_iter().map(|id| id + base_id).collect();
            self.trigram.merge_posting(tri_hash, shifted);
        }
    }

    pub fn finalize(mut self) -> Index {
        let mut ext_map: HashMap<Box<[u8]>, Vec<u32>> = HashMap::with_capacity(256);
        for (ext, id) in self.extensions {
            ext_map.entry(ext).or_default().push(id);
        }

        for ids in ext_map.values_mut() {
            ids.sort_unstable();
            ids.dedup();
        }

        self.trigram.finalize();

        Index {
            entries: self.entries,
            arena: self.arena,
            ext_map,
            trigram: self.trigram,
        }
    }
}