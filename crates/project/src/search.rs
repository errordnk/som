use aho_corasick::{AhoCorasick, AhoCorasickBuilder};
use anyhow::Result;
use fancy_regex::{Regex, RegexBuilder};
use std::{
    borrow::Cow,
    ops::Range,
    sync::{Arc, LazyLock},
};
use util::paths::PathMatcher;

#[derive(Clone, Copy, PartialEq)]
pub enum SearchInputKind {
    Query,
    Include,
    Exclude,
}

#[derive(Clone, Debug)]
pub struct SearchInputs {
    query: Arc<str>,
    files_to_include: PathMatcher,
    files_to_exclude: PathMatcher,
    match_full_paths: bool,
}

impl SearchInputs {
    pub fn as_str(&self) -> &str {
        self.query.as_ref()
    }
    pub fn files_to_include(&self) -> &PathMatcher {
        &self.files_to_include
    }
    pub fn files_to_exclude(&self) -> &PathMatcher {
        &self.files_to_exclude
    }
    pub fn match_full_paths(&self) -> bool {
        self.match_full_paths
    }
}

#[derive(Clone, Debug)]
pub enum SearchQuery {
    Text {
        search: AhoCorasick,
        replacement: Option<String>,
        whole_word: bool,
        case_sensitive: bool,
        include_ignored: bool,
        inner: SearchInputs,
    },
    Regex {
        regex: Regex,
        replacement: Option<String>,
        multiline: bool,
        whole_word: bool,
        case_sensitive: bool,
        include_ignored: bool,
        one_match_per_line: bool,
        inner: SearchInputs,
    },
}

static WORD_MATCH_TEST: LazyLock<Regex> = LazyLock::new(|| {
    RegexBuilder::new(r"\B")
        .build()
        .expect("Failed to create WORD_MATCH_TEST")
});

impl SearchQuery {
    pub fn text(
        query: impl ToString,
        whole_word: bool,
        case_sensitive: bool,
        include_ignored: bool,
        files_to_include: PathMatcher,
        files_to_exclude: PathMatcher,
        match_full_paths: bool,
    ) -> Result<Self> {
        let query = query.to_string();
        if !case_sensitive && !query.is_ascii() {
            return Self::escaped_regex(
                query,
                whole_word,
                case_sensitive,
                include_ignored,
                files_to_include,
                files_to_exclude,
                false,
            );
        }
        let search = AhoCorasickBuilder::new()
            .ascii_case_insensitive(!case_sensitive)
            .build([&query])?;
        let inner = SearchInputs {
            query: query.into(),
            files_to_exclude,
            files_to_include,
            match_full_paths,
        };
        Ok(Self::Text {
            search,
            replacement: None,
            whole_word,
            case_sensitive,
            include_ignored,
            inner,
        })
    }

    pub fn regex(
        query: impl ToString,
        whole_word: bool,
        case_sensitive: bool,
        include_ignored: bool,
        one_match_per_line: bool,
        files_to_include: PathMatcher,
        files_to_exclude: PathMatcher,
        match_full_paths: bool,
    ) -> Result<Self> {
        let query = query.to_string();
        let inner = SearchInputs {
            query: Arc::from(query.as_str()),
            files_to_include,
            files_to_exclude,
            match_full_paths,
        };
        Self::build_regex(query, whole_word, case_sensitive, include_ignored, one_match_per_line, inner)
    }

    pub fn escaped_regex(
        query: impl ToString,
        whole_word: bool,
        case_sensitive: bool,
        include_ignored: bool,
        files_to_include: PathMatcher,
        files_to_exclude: PathMatcher,
        match_full_paths: bool,
    ) -> Result<Self> {
        let query = query.to_string();
        let inner = SearchInputs {
            query: Arc::from(query.as_str()),
            files_to_include,
            files_to_exclude,
            match_full_paths,
        };
        Self::build_regex(
            regex::escape(&query),
            whole_word,
            case_sensitive,
            include_ignored,
            false,
            inner,
        )
    }

    fn build_regex(
        query: String,
        whole_word: bool,
        case_sensitive: bool,
        include_ignored: bool,
        one_match_per_line: bool,
        inner: SearchInputs,
    ) -> Result<Self> {
        let mut builder = RegexBuilder::new(&query);
        builder.case_insensitive(!case_sensitive);
        builder.multi_line(true);
        let multiline = query.contains('\n');
        if !multiline {
            builder.dot_matches_new_line(false);
        }
        let regex = builder.build()?;
        Ok(Self::Regex {
            regex,
            replacement: None,
            multiline,
            whole_word,
            case_sensitive,
            include_ignored,
            one_match_per_line,
            inner,
        })
    }

    pub fn as_str(&self) -> &str {
        self.as_inner().as_str()
    }

    pub fn is_regex(&self) -> bool {
        matches!(self, Self::Regex { .. })
    }

    pub fn whole_word(&self) -> bool {
        match self {
            Self::Text { whole_word, .. } => *whole_word,
            Self::Regex { whole_word, .. } => *whole_word,
        }
    }

    pub fn case_sensitive(&self) -> bool {
        match self {
            Self::Text { case_sensitive, .. } => *case_sensitive,
            Self::Regex { case_sensitive, .. } => *case_sensitive,
        }
    }

    pub fn include_ignored(&self) -> bool {
        match self {
            Self::Text { include_ignored, .. } => *include_ignored,
            Self::Regex { include_ignored, .. } => *include_ignored,
        }
    }

    pub fn files_to_include(&self) -> &PathMatcher {
        self.as_inner().files_to_include()
    }

    pub fn files_to_exclude(&self) -> &PathMatcher {
        self.as_inner().files_to_exclude()
    }

    pub fn match_full_paths(&self) -> bool {
        self.as_inner().match_full_paths()
    }

    pub fn replacement(&self) -> Option<&str> {
        match self {
            Self::Text { replacement, .. } => replacement.as_deref(),
            Self::Regex { replacement, .. } => replacement.as_deref(),
        }
    }

    pub fn with_replacement(mut self, new_replacement: String) -> Self {
        match &mut self {
            Self::Text { replacement, .. } => *replacement = Some(new_replacement),
            Self::Regex { replacement, .. } => *replacement = Some(new_replacement),
        }
        self
    }

    pub fn detect<T: Eq + ToString>(
        &self,
        text: T,
        range: Range<usize>,
    ) -> Option<Range<usize>> {
        let text_str = text.to_string();
        let text_bytes = text_str.as_bytes();
        match self {
            Self::Text { search, whole_word, case_sensitive, .. } => {
                let slice = &text_bytes[range.clone()];
                let found = search.find(slice).map(|m| range.start + m.start()..range.start + m.end())?;
                if *whole_word && !self.is_whole_word(&text_str, found.clone()) {
                    return None;
                }
                let _ = case_sensitive;
                Some(found)
            }
            Self::Regex { regex, whole_word, .. } => {
                let slice = &text_str[range.clone()];
                let found = regex.find(slice).ok().flatten().map(|m| range.start + m.start()..range.start + m.end())?;
                if *whole_word && !self.is_whole_word(&text_str, found.clone()) {
                    return None;
                }
                Some(found)
            }
        }
    }

    fn is_whole_word(&self, text: &str, range: Range<usize>) -> bool {
        !WORD_MATCH_TEST.is_match(&text[range]).unwrap_or(false)
    }

    fn as_inner(&self) -> &SearchInputs {
        match self {
            Self::Text { inner, .. } => inner,
            Self::Regex { inner, .. } => inner,
        }
    }

    pub fn as_str_with_type(&self) -> Cow<'_, str> {
        Cow::Borrowed(self.as_str())
    }
}
