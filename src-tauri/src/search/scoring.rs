#[inline(always)]
pub fn calculate_score(name_len: usize, query_len: usize, match_pos: usize) -> f32 {
    if name_len == query_len && match_pos == 0 {
        return 100.0;
    }
    if match_pos == 0 {
        return 90.0 + (10.0 * query_len as f32 / name_len as f32);
    }
    let pos_penalty = (match_pos as f32 * 2.0).min(30.0);
    let len_bonus = (query_len as f32 / name_len as f32) * 20.0;
    60.0 - pos_penalty + len_bonus
}