#[test]
fn matches_layout_markdown_on_real_fixture() {
    let source = std::fs::read_to_string("../../markdown.md").unwrap();
    let count = markdown_line_count::count_rendered_lines(&source);
    // Expected value cross-checked directly against
    // `terminal_view::markdown_styling::layout_markdown(&source).len()`
    // on this same file (326) — kept as a plain integration test here
    // since this crate can't depend on `terminal_view` (GPUI) to check
    // itself live every run.
    assert_eq!(count, 326);
}
