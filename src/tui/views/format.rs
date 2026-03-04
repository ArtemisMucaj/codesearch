/// Shorten a file path to at most the last two components for display.
///
/// `/a/b/c/d.rs` → `…/c/d.rs`; short paths are returned unchanged.
pub(super) fn shorten_path(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() <= 2 {
        return path.to_string();
    }
    format!("…/{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
}

/// Return the last segment of a fully-qualified symbol name.
///
/// Splits on `/`, `:`, and `.` and returns the rightmost non-empty component.
pub(super) fn short_symbol(fq: &str) -> &str {
    fq.rsplit(&['/', ':', '.']).next().unwrap_or(fq)
}
