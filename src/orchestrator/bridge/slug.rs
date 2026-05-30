//! Slug sanitization shared by the delegate and progress tool calls.

/// Normalize a free-form slug into lowercase `snake_case`, collapsing runs of
/// non-alphanumeric characters into single underscores. Returns `None` when the
/// input contains no usable characters.
pub(super) fn sanitize_slug(value: &str) -> Option<String> {
    let mut slug = String::new();
    let mut last_was_separator = false;
    for ch in value.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_separator = false;
        } else if !last_was_separator && !slug.is_empty() {
            slug.push('_');
            last_was_separator = true;
        }
    }
    while slug.ends_with('_') {
        slug.pop();
    }
    (!slug.is_empty()).then_some(slug)
}
