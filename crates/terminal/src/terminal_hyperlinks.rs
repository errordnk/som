use alacritty_terminal::{
    Term,
    event::EventListener,
    index::{Boundary, Direction as AlacDirection, Point as AlacPoint},
    term::{
        search::{Match, RegexIter, RegexSearch},
    },
};
use std::ops::Index;

const URL_REGEX: &str = r#"(ipfs:|ipns:|magnet:|mailto:|gemini://|gopher://|https://|http://|news:|file://|git://|ssh:|ftp://)[^\u{0000}-\u{001F}\u{007F}-\u{009F}<>"\s{-}\^⟨⟩`']+"#;
pub(super) struct RegexSearches {
    url_regex: RegexSearch,
}

impl Default for RegexSearches {
    fn default() -> Self {
        Self { url_regex: RegexSearch::new(URL_REGEX).unwrap() }
    }
}

/// Whether Som can do something with a link: a link to a file cannot be
/// opened, there is no editor to open it in.
fn opens_in_som(url: &str) -> bool {
    !url.starts_with("file://")
}

/// The URL under `point` - the target of an OSC 8 hyperlink, or what the URL
/// regex finds on the line - and where it is. A link to a file is not one:
/// Som has no editor to open it in.
pub(super) fn find_from_grid_point<T: EventListener>(
    term: &Term<T>,
    point: AlacPoint,
    regex_searches: &mut RegexSearches,
) -> Option<(String, Match)> {
    let grid = term.grid();
    let link = grid.index(point).hyperlink();
    let (url, url_match) = if let Some(ref url) = link {
        let mut min_index = point;
        loop {
            let new_min_index = min_index.sub(term, Boundary::Cursor, 1);
            if new_min_index == min_index || grid.index(new_min_index).hyperlink() != link {
                break;
            } else {
                min_index = new_min_index
            }
        }

        let mut max_index = point;
        loop {
            let new_max_index = max_index.add(term, Boundary::Cursor, 1);
            if new_max_index == max_index || grid.index(new_max_index).hyperlink() != link {
                break;
            } else {
                max_index = new_max_index
            }
        }

        (url.uri().to_owned(), min_index..=max_index)
    } else {
        let (line_start, line_end) = (term.line_search_left(point), term.line_search_right(point));
        RegexIter::new(line_start, line_end, AlacDirection::Right, term, &mut regex_searches.url_regex)
            .find(|rm| rm.contains(&point))
            .map(|url_match| {
                let url = term.bounds_to_string(*url_match.start(), *url_match.end());
                sanitize_url_punctuation(url, url_match, term)
            })?
    };
    opens_in_som(&url).then_some((url, url_match))
}

fn sanitize_url_punctuation<T: EventListener>(
    url: String,
    url_match: Match,
    term: &Term<T>,
) -> (String, Match) {
    let mut sanitized_url = url;
    let mut chars_trimmed = 0;

    // Count parentheses in the URL
    let (open_parens, mut close_parens) =
        sanitized_url
            .chars()
            .fold((0, 0), |(opens, closes), c| match c {
                '(' => (opens + 1, closes),
                ')' => (opens, closes + 1),
                _ => (opens, closes),
            });

    // Remove trailing characters that shouldn't be at the end of URLs
    while let Some(last_char) = sanitized_url.chars().last() {
        let should_remove = match last_char {
            // These may be part of a URL but not at the end. It's not that the spec
            // doesn't allow them, but they are frequently used in plain text as delimiters
            // where they're not meant to be part of the URL.
            '.' | ',' | ':' | ';' => true,
            '(' => true,
            ')' if close_parens > open_parens => {
                close_parens -= 1;

                true
            }
            _ => false,
        };

        if should_remove {
            sanitized_url.pop();
            chars_trimmed += 1;
        } else {
            break;
        }
    }

    if chars_trimmed > 0 {
        let new_end = url_match.end().sub(term, Boundary::Grid, chars_trimmed);
        let sanitized_match = Match::new(*url_match.start(), new_end);
        (sanitized_url, sanitized_match)
    } else {
        (sanitized_url, url_match)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::{
        event::VoidListener,
        grid::Dimensions,
        index::{Boundary, Column, Line, Point as AlacPoint},
        term::{Config, cell::Flags, test::TermSize},
        vte::ansi::Handler,
    };
    use regex::Regex;
    use std::{cell::RefCell, ops::RangeInclusive};
    use url::Url;

    fn re_test(re: &str, hay: &str, expected: Vec<&str>) {
        let results: Vec<_> = Regex::new(re)
            .unwrap()
            .find_iter(hay)
            .map(|m| m.as_str())
            .collect();
        assert_eq!(results, expected);
    }

    #[test]
    fn test_url_regex() {
        re_test(
            URL_REGEX,
            "test http://example.com test 'https://website1.com' test mailto:bob@example.com train",
            vec![
                "http://example.com",
                "https://website1.com",
                "mailto:bob@example.com",
            ],
        );
    }

    #[test]
    fn test_url_parentheses_sanitization() {
        // Test our sanitize_url_parentheses function directly
        let test_cases = vec![
            // Cases that should be sanitized (unbalanced parentheses)
            ("https://www.google.com/)", "https://www.google.com/"),
            ("https://example.com/path)", "https://example.com/path"),
            ("https://test.com/))", "https://test.com/"),
            ("https://test.com/(((", "https://test.com/"),
            ("https://test.com/(test)(", "https://test.com/(test)"),
            // Cases that should NOT be sanitized (balanced parentheses)
            (
                "https://en.wikipedia.org/wiki/Example_(disambiguation)",
                "https://en.wikipedia.org/wiki/Example_(disambiguation)",
            ),
            ("https://test.com/(hello)", "https://test.com/(hello)"),
            (
                "https://example.com/path(1)(2)",
                "https://example.com/path(1)(2)",
            ),
            // Edge cases
            ("https://test.com/", "https://test.com/"),
            ("https://example.com", "https://example.com"),
        ];

        for (input, expected) in test_cases {
            // Create a minimal terminal for testing
            let term = Term::new(Config::default(), &TermSize::new(80, 24), VoidListener);

            // Create a dummy match that spans the entire input
            let start_point = AlacPoint::new(Line(0), Column(0));
            let end_point = AlacPoint::new(Line(0), Column(input.len()));
            let dummy_match = Match::new(start_point, end_point);

            let (result, _) = sanitize_url_punctuation(input.to_string(), dummy_match, &term);
            assert_eq!(result, expected, "Failed for input: {}", input);
        }
    }

    #[test]
    fn test_url_punctuation_sanitization() {
        // Test URLs with trailing punctuation (sentence/text punctuation)
        // The sanitize_url_punctuation function removes ., ,, :, ;, from the end
        let test_cases = vec![
            ("https://example.com.", "https://example.com"),
            (
                "https://github.com/zed-industries/zed.",
                "https://github.com/zed-industries/zed",
            ),
            (
                "https://example.com/path/file.html.",
                "https://example.com/path/file.html",
            ),
            (
                "https://example.com/file.pdf.",
                "https://example.com/file.pdf",
            ),
            ("https://example.com:8080.", "https://example.com:8080"),
            ("https://example.com..", "https://example.com"),
            (
                "https://en.wikipedia.org/wiki/C.E.O.",
                "https://en.wikipedia.org/wiki/C.E.O",
            ),
            ("https://example.com,", "https://example.com"),
            ("https://example.com/path,", "https://example.com/path"),
            ("https://example.com,,", "https://example.com"),
            ("https://example.com:", "https://example.com"),
            ("https://example.com/path:", "https://example.com/path"),
            ("https://example.com::", "https://example.com"),
            ("https://example.com;", "https://example.com"),
            ("https://example.com/path;", "https://example.com/path"),
            ("https://example.com;;", "https://example.com"),
            ("https://example.com.,", "https://example.com"),
            ("https://example.com.:;", "https://example.com"),
            ("https://example.com!.", "https://example.com!"),
            ("https://example.com/).", "https://example.com/"),
            ("https://example.com/);", "https://example.com/"),
            ("https://example.com/;)", "https://example.com/"),
            (
                "https://example.com/v1.0/api",
                "https://example.com/v1.0/api",
            ),
            ("https://192.168.1.1", "https://192.168.1.1"),
            ("https://sub.domain.com", "https://sub.domain.com"),
            (
                "https://example.com?query=value",
                "https://example.com?query=value",
            ),
            ("https://example.com?a=1&b=2", "https://example.com?a=1&b=2"),
            (
                "https://example.com/path:8080",
                "https://example.com/path:8080",
            ),
        ];

        for (input, expected) in test_cases {
            // Create a minimal terminal for testing
            let term = Term::new(Config::default(), &TermSize::new(80, 24), VoidListener);

            // Create a dummy match that spans the entire input
            let start_point = AlacPoint::new(Line(0), Column(0));
            let end_point = AlacPoint::new(Line(0), Column(input.len()));
            let dummy_match = Match::new(start_point, end_point);

            let (result, _) = sanitize_url_punctuation(input.to_string(), dummy_match, &term);
            assert_eq!(result, expected, "Failed for input: {}", input);
        }
    }

    macro_rules! test_hyperlink {
        ($($lines:expr),+; $hyperlink_kind:ident) => { {
            use crate::terminal_hyperlinks::tests::line_cells_count;
            use std::cmp;

            let test_lines = vec![$($lines),+];
            let (total_cells, longest_line_cells) =
                test_lines.iter().copied()
                    .map(line_cells_count)
                    .fold((0, 0), |state, cells| (state.0 + cells, cmp::max(state.1, cells)));
            let contains_tab_char = test_lines.iter().copied()
                .map(str::chars).flatten().find(|&c| c == '\t');
            let columns = if contains_tab_char.is_some() {
                // This avoids tabs at end of lines causing whitespace-eating line wraps...
                vec![longest_line_cells + 1]
            } else {
                // Alacritty has issues with 2 columns, use 3 as the minimum for now.
                vec![3, longest_line_cells / 2, longest_line_cells + 1]
            };
            test_hyperlink!(
                columns;
                total_cells;
                test_lines.iter().copied();
                $hyperlink_kind
            )
        } };

        ($columns:expr; $total_cells:expr; $lines:expr; $hyperlink_kind:ident) => { {
            use crate::terminal_hyperlinks::tests::{ test_hyperlink, HyperlinkKind };

            let source_location = format!("{}:{}", std::file!(), std::line!());
            for columns in $columns {
                test_hyperlink(columns, $total_cells, $lines, HyperlinkKind::$hyperlink_kind,
                    &source_location);
            }
        } };
    }

    #[test]
    fn a_link_to_a_file_is_no_hyperlink_but_a_web_link_is() {
        assert!(!opens_in_som("file:///home/user/notes.txt"));
        assert!(opens_in_som("https://example.com/page"));
        assert!(opens_in_som("mailto:someone@example.com"));
    }

    mod iri {
        /// [**`c₀, c₁, …, cₙ;`**]ₒₚₜ := use specified terminal widths of `c₀, c₁, …, cₙ` **columns**
        /// (defaults to `3, longest_line_cells / 2, longest_line_cells + 1;`)
        ///
        macro_rules! test_iri {
            ($iri:literal) => { { test_hyperlink!(concat!("‹«👉", $iri, "»›"); Iri) } };
        }

        #[test]
        fn simple() {
            // In the order they appear in URL_REGEX, except 'file://' which is treated as a path
            test_iri!("ipfs://test/cool.ipfs");
            test_iri!("ipns://test/cool.ipns");
            test_iri!("magnet://test/cool.git");
            test_iri!("mailto:someone@somewhere.here");
            test_iri!("gemini://somewhere.here");
            test_iri!("gopher://somewhere.here");
            test_iri!("http://test/cool/index.html");
            test_iri!("http://10.10.10.10:1111/cool.html");
            test_iri!("http://test/cool/index.html?amazing=1");
            test_iri!("http://test/cool/index.html#right%20here");
            test_iri!("http://test/cool/index.html?amazing=1#right%20here");
            test_iri!("https://test/cool/index.html");
            test_iri!("https://10.10.10.10:1111/cool.html");
            test_iri!("https://test/cool/index.html?amazing=1");
            test_iri!("https://test/cool/index.html#right%20here");
            test_iri!("https://test/cool/index.html?amazing=1#right%20here");
            test_iri!("news://test/cool.news");
            test_iri!("git://test/cool.git");
            test_iri!("ssh://user@somewhere.over.here:12345/test/cool.git");
            test_iri!("ftp://test/cool.ftp");
        }

        #[test]
        fn wide_chars() {
            // In the order they appear in URL_REGEX, except 'file://' which is treated as a path
            test_iri!("ipfs://例🏃🦀/cool.ipfs");
            test_iri!("ipns://例🏃🦀/cool.ipns");
            test_iri!("magnet://例🏃🦀/cool.git");
            test_iri!("mailto:someone@somewhere.here");
            test_iri!("gemini://somewhere.here");
            test_iri!("gopher://somewhere.here");
            test_iri!("http://例🏃🦀/cool/index.html");
            test_iri!("http://10.10.10.10:1111/cool.html");
            test_iri!("http://例🏃🦀/cool/index.html?amazing=1");
            test_iri!("http://例🏃🦀/cool/index.html#right%20here");
            test_iri!("http://例🏃🦀/cool/index.html?amazing=1#right%20here");
            test_iri!("https://例🏃🦀/cool/index.html");
            test_iri!("https://10.10.10.10:1111/cool.html");
            test_iri!("https://例🏃🦀/cool/index.html?amazing=1");
            test_iri!("https://例🏃🦀/cool/index.html#right%20here");
            test_iri!("https://例🏃🦀/cool/index.html?amazing=1#right%20here");
            test_iri!("news://例🏃🦀/cool.news");
            test_iri!("git://例/cool.git");
            test_iri!("ssh://user@somewhere.over.here:12345/例🏃🦀/cool.git");
            test_iri!("ftp://例🏃🦀/cool.ftp");
        }

        // There are likely more tests needed for IRI vs URI
        #[test]
        fn iris() {
            // These refer to the same location, see example here:
            // <https://en.wikipedia.org/wiki/Internationalized_Resource_Identifier#Compatibility>
            test_iri!("https://en.wiktionary.org/wiki/Ῥόδος"); // IRI
            test_iri!("https://en.wiktionary.org/wiki/%E1%BF%AC%CF%8C%CE%B4%CE%BF%CF%82"); // URI
        }
    }

    #[derive(Debug, PartialEq)]
    enum HyperlinkKind {
        FileIri,
        Iri,
        Path,
    }

    const WIDE_CHAR_SPACERS: Flags =
        Flags::from_bits(Flags::LEADING_WIDE_CHAR_SPACER.bits() | Flags::WIDE_CHAR_SPACER.bits()).unwrap();

    struct ExpectedHyperlink {
        hovered_grid_point: AlacPoint,
        hovered_char: char,
        hyperlink_kind: HyperlinkKind,
        iri_or_path: String,
        hyperlink_match: RangeInclusive<AlacPoint>,
    }

    /// Converts to Windows style paths on Windows, like path!(), but at runtime for improved test
    /// readability.
    fn build_term_from_test_lines<'a>(
        hyperlink_kind: HyperlinkKind,
        term_size: TermSize,
        test_lines: impl Iterator<Item = &'a str>,
    ) -> (Term<VoidListener>, ExpectedHyperlink) {
        #[derive(Default, Eq, PartialEq)]
        enum HoveredState {
            #[default]
            HoveredScan,
            HoveredNextChar,
            Done,
        }

        #[derive(Default, Eq, PartialEq)]
        enum MatchState {
            #[default]
            MatchScan,
            MatchNextChar,
            Match(AlacPoint),
            Done,
        }

        #[derive(Default, Eq, PartialEq)]
        enum CapturesState {
            #[default]
            PathScan,
            PathNextChar,
            Path(AlacPoint),
            RowScan,
            Row(String),
            ColumnScan,
            Column(String),
            Done,
        }

        fn prev_input_point_from_term(term: &Term<VoidListener>) -> AlacPoint {
            let grid = term.grid();
            let cursor = &grid.cursor;
            let mut point = cursor.point;

            if !cursor.input_needs_wrap {
                point = point.sub(term, Boundary::Grid, 1);
            }

            if grid.index(point).flags.contains(Flags::WIDE_CHAR_SPACER) {
                point.column -= 1;
            }

            point
        }

        fn end_point_from_prev_input_point(
            term: &Term<VoidListener>,
            prev_input_point: AlacPoint,
        ) -> AlacPoint {
            if term
                .grid()
                .index(prev_input_point)
                .flags
                .contains(Flags::WIDE_CHAR)
            {
                prev_input_point.add(term, Boundary::Grid, 1)
            } else {
                prev_input_point
            }
        }

        fn process_input(term: &mut Term<VoidListener>, c: char) {
            match c {
                '\t' => term.put_tab(1),
                c @ _ => term.input(c),
            }
        }

        let mut hovered_grid_point: Option<AlacPoint> = None;
        let mut hyperlink_match = AlacPoint::default()..=AlacPoint::default();
        let mut iri_or_path = String::default();
        let mut prev_input_point = AlacPoint::default();
        let mut hovered_state = HoveredState::default();
        let mut match_state = MatchState::default();
        let mut captures_state = CapturesState::default();
        let mut term = Term::new(Config::default(), &term_size, VoidListener);

        for text in test_lines {
            let chars: Box<dyn Iterator<Item = char>> =
                if cfg!(windows) && hyperlink_kind == HyperlinkKind::Path {
                    Box::new(text.chars().map(|c| if c == '/' { '\\' } else { c })) as _
                } else {
                    Box::new(text.chars()) as _
                };
            let mut chars = chars.peekable();
            while let Some(c) = chars.next() {
                match c {
                    '👉' => {
                        hovered_state = HoveredState::HoveredNextChar;
                    }
                    '👈' => {
                        hovered_grid_point = Some(prev_input_point.add(&term, Boundary::Grid, 1));
                    }
                    '«' | '»' => {
                        captures_state = match captures_state {
                            CapturesState::PathScan => CapturesState::PathNextChar,
                            CapturesState::PathNextChar => {
                                panic!("Should have been handled by char input")
                            }
                            CapturesState::Path(start_point) => {
                                iri_or_path = term.bounds_to_string(
                                    start_point,
                                    end_point_from_prev_input_point(&term, prev_input_point),
                                );
                                CapturesState::RowScan
                            }
                            CapturesState::RowScan => CapturesState::Row(String::new()),
                            CapturesState::Row(_) => CapturesState::ColumnScan,
                            CapturesState::ColumnScan => CapturesState::Column(String::new()),
                            CapturesState::Column(_) => CapturesState::Done,
                            CapturesState::Done => {
                                panic!("Extra '«', '»'")
                            }
                        }
                    }
                    '‹' | '›' => {
                        match_state = match match_state {
                            MatchState::MatchScan => MatchState::MatchNextChar,
                            MatchState::MatchNextChar => {
                                panic!("Should have been handled by char input")
                            }
                            MatchState::Match(start_point) => {
                                hyperlink_match = start_point
                                    ..=end_point_from_prev_input_point(&term, prev_input_point);
                                MatchState::Done
                            }
                            MatchState::Done => {
                                panic!("Extra '‹', '›'")
                            }
                        }
                    }
                    _ => {
                        if let CapturesState::Row(number) | CapturesState::Column(number) =
                            &mut captures_state
                        {
                            number.push(c)
                        }

                        let is_windows_abs_path_start = captures_state
                            == CapturesState::PathNextChar
                            && cfg!(windows)
                            && hyperlink_kind == HyperlinkKind::Path
                            && c == '\\'
                            && chars.peek().is_some_and(|c| *c != '\\');

                        if is_windows_abs_path_start {
                            // Convert Unix abs path start into Windows abs path start so that the
                            // same test can be used for both OSes.
                            term.input('C');
                            prev_input_point = prev_input_point_from_term(&term);
                            term.input(':');
                            process_input(&mut term, c);
                        } else {
                            process_input(&mut term, c);
                            prev_input_point = prev_input_point_from_term(&term);
                        }

                        if hovered_state == HoveredState::HoveredNextChar {
                            hovered_grid_point = Some(prev_input_point);
                            hovered_state = HoveredState::Done;
                        }
                        if captures_state == CapturesState::PathNextChar {
                            captures_state = CapturesState::Path(prev_input_point);
                        }
                        if match_state == MatchState::MatchNextChar {
                            match_state = MatchState::Match(prev_input_point);
                        }
                    }
                }
            }
            term.move_down_and_cr(1);
        }

        if hyperlink_kind == HyperlinkKind::FileIri {
            let Ok(url) = Url::parse(&iri_or_path) else {
                panic!("Failed to parse file IRI `{iri_or_path}`");
            };
            let Ok(path) = url.to_file_path() else {
                panic!("Failed to interpret file IRI `{iri_or_path}` as a path");
            };
            iri_or_path = path.to_string_lossy().into_owned();
        }

        let hovered_grid_point = hovered_grid_point.expect("Missing hovered point (👉 or 👈)");
        let hovered_char = term.grid().index(hovered_grid_point).c;
        (
            term,
            ExpectedHyperlink {
                hovered_grid_point,
                hovered_char,
                hyperlink_kind,
                iri_or_path,
                hyperlink_match,
            },
        )
    }

    fn line_cells_count(line: &str) -> usize {
        // This avoids taking a dependency on the unicode-width crate
        fn width(c: char) -> usize {
            match c {
                // Fullwidth unicode characters used in tests
                '例' | '🏃' | '🦀' | '🔥' => 2,
                '\t' => 8, // it's really 0-8, use the max always
                _ => 1,
            }
        }
        const CONTROL_CHARS: &str = "‹«👉👈»›";
        line.chars()
            .filter(|c| !CONTROL_CHARS.contains(*c))
            .map(width)
            .sum::<usize>()
    }

    struct CheckHyperlinkMatch<'a> {
        term: &'a Term<VoidListener>,
        expected_hyperlink: &'a ExpectedHyperlink,
        source_location: &'a str,
    }

    impl<'a> CheckHyperlinkMatch<'a> {
        fn new(
            term: &'a Term<VoidListener>,
            expected_hyperlink: &'a ExpectedHyperlink,
            source_location: &'a str,
        ) -> Self {
            Self {
                term,
                expected_hyperlink,
                source_location,
            }
        }

        fn check_iri_and_match(&self, iri: String, hyperlink_match: &Match) {
            let format_iri_and_match = |iri: &String, hyperlink_match: &Match| {
                format!(
                    "Url = «{iri}», at grid cells {}",
                    Self::format_hyperlink_match(hyperlink_match)
                )
            };

            assert_eq!(
                self.expected_hyperlink.hyperlink_kind,
                HyperlinkKind::Iri,
                "\n    at {}\nExpected a iri, but was a path:\n{}",
                self.source_location,
                self.format_renderable_content()
            );

            assert_eq!(
                format_iri_and_match(
                    &self.expected_hyperlink.iri_or_path,
                    &self.expected_hyperlink.hyperlink_match
                ),
                format_iri_and_match(&iri, hyperlink_match),
                "\n    at {}:\n{}",
                self.source_location,
                self.format_renderable_content()
            );
        }

        fn format_hyperlink_match(hyperlink_match: &Match) -> String {
            format!(
                "({}, {})..=({}, {})",
                hyperlink_match.start().line.0,
                hyperlink_match.start().column.0,
                hyperlink_match.end().line.0,
                hyperlink_match.end().column.0
            )
        }

        fn format_renderable_content(&self) -> String {
            let mut result = format!("\nHovered on '{}'\n", self.expected_hyperlink.hovered_char);

            let mut first_header_row = String::new();
            let mut second_header_row = String::new();
            let mut marker_header_row = String::new();
            for index in 0..self.term.columns() {
                let remainder = index % 10;
                if index > 0 && remainder == 0 {
                    first_header_row.push_str(&format!("{:>10}", (index / 10)));
                }
                second_header_row += &remainder.to_string();
                if index == self.expected_hyperlink.hovered_grid_point.column.0 {
                    marker_header_row.push('↓');
                } else {
                    marker_header_row.push(' ');
                }
            }

            let remainder = (self.term.columns() - 1) % 10;
            if remainder != 0 {
                first_header_row.push_str(&" ".repeat(remainder));
            }

            result += &format!("\n      [ {}]\n", first_header_row);
            result += &format!("      [{}]\n", second_header_row);
            result += &format!("       {}", marker_header_row);

            for cell in self
                .term
                .renderable_content()
                .display_iter
                .filter(|cell| !cell.flags.intersects(WIDE_CHAR_SPACERS))
            {
                if cell.point.column.0 == 0 {
                    let prefix =
                        if cell.point.line == self.expected_hyperlink.hovered_grid_point.line {
                            '→'
                        } else {
                            ' '
                        };
                    result += &format!("\n{prefix}[{:>3}] ", cell.point.line.to_string());
                }

                match cell.c {
                    '\t' => result.push(' '),
                    c @ _ => result.push(c),
                }
            }

            result
        }
    }

    fn test_hyperlink<'a>(
        columns: usize,
        total_cells: usize,
        test_lines: impl Iterator<Item = &'a str>,
        hyperlink_kind: HyperlinkKind,
        source_location: &str,
    ) {
        thread_local! {
            static TEST_REGEX_SEARCHES: RefCell<RegexSearches> = RefCell::new(RegexSearches::default());
        }

        let term_size = TermSize::new(columns, total_cells / columns + 2);
        let (term, expected_hyperlink) =
            build_term_from_test_lines(hyperlink_kind, term_size, test_lines);
        let hyperlink_found = TEST_REGEX_SEARCHES.with(|regex_searches| {
            find_from_grid_point(&term, expected_hyperlink.hovered_grid_point, &mut regex_searches.borrow_mut())
        });
        let check_hyperlink_match =
            CheckHyperlinkMatch::new(&term, &expected_hyperlink, source_location);
        match hyperlink_found {
            Some((hyperlink_word, hyperlink_match)) => {
                check_hyperlink_match.check_iri_and_match(hyperlink_word, &hyperlink_match);
            }
            None => {
                if expected_hyperlink.hyperlink_match.start()
                    != expected_hyperlink.hyperlink_match.end()
                {
                    assert!(
                        false,
                        "No hyperlink found\n     at {source_location}:\n{}",
                        check_hyperlink_match.format_renderable_content()
                    )
                }
            }
        }
    }
}
