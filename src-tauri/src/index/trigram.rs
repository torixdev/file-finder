use ahash::AHashMap as HashMap;

pub struct TrigramIndex {
    posting: HashMap<u32, Vec<u32>>,
}

impl TrigramIndex {
    pub fn new() -> Self {
        Self {
            posting: HashMap::with_capacity(32768),
        }
    }

    pub fn insert(&mut self, id: u32, name_lower: &[u8]) {
        if name_lower.len() < 3 {
            return;
        }
        let mut prev_tri: u32 = u32::MAX;
        for window in name_lower.windows(3) {
            let tri = trigram_hash(window);
            if tri != prev_tri {
                self.posting.entry(tri).or_default().push(id);
                prev_tri = tri;
            }
        }
    }

    pub fn finalize(&mut self) {
        for list in self.posting.values_mut() {
            list.sort_unstable();
            list.dedup();
        }
    }

    pub fn take_posting(self) -> HashMap<u32, Vec<u32>> {
        self.posting
    }

    pub fn merge_posting(&mut self, key: u32, mut ids: Vec<u32>) {
        self.posting
            .entry(key)
            .or_default()
            .append(&mut ids);
    }

    pub fn query(&self, query_lower: &[u8]) -> Option<Vec<u32>> {
        if query_lower.len() < 3 {
            return None;
        }

        let mut trigrams: Vec<u32> = Vec::with_capacity(query_lower.len() - 2);
        let mut prev: u32 = u32::MAX;
        for w in query_lower.windows(3) {
            let tri = trigram_hash(w);
            if tri != prev {
                trigrams.push(tri);
                prev = tri;
            }
        }

        if trigrams.is_empty() {
            return None;
        }

        let mut lists: Vec<&[u32]> = Vec::with_capacity(trigrams.len());
        for &tri in &trigrams {
            match self.posting.get(&tri) {
                Some(list) => lists.push(list),
                None => return Some(Vec::new()),
            }
        }

        lists.sort_unstable_by_key(|l| l.len());

        let mut result = lists[0].to_vec();
        for &list in &lists[1..] {
            result = intersect_sorted(&result, list);
            if result.is_empty() {
                break;
            }
        }

        Some(result)
    }
}

#[inline(always)]
fn trigram_hash(bytes: &[u8]) -> u32 {
    (bytes[0] as u32) | ((bytes[1] as u32) << 8) | ((bytes[2] as u32) << 16)
}

fn intersect_sorted(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut result = Vec::with_capacity(a.len().min(b.len()));
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                result.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    result
}