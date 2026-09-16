//! Paint-ready state for `ContentType::Markdown` placements — the
//! markdown counterpart to [`crate::rich_content_player::RichContentPlayer`]
//! (images) and [`crate::rich_content_audio_player::RichContentAudioPlayer`]
//! (audio). See `SRP_LUA.md`'s "Phase 1" section for the design this
//! implements.
//!
//! Deliberately much simpler than either of those: markdown text has no
//! frames to advance and no device stream to own, so there's no
//! background thread here at all — just "read however many contiguous
//! bytes are cached, run them through the frontend Lua `render()` call
//! (see [`crate::rich_content_lua_frontend`]), remember the result so a
//! paint call between chunk arrivals doesn't redo that work."

/// One markdown placement's cached render state.
pub struct RichContentMarkdownPlayer {
    /// The frontend-rendered text ready for [`crate::markdown::Markdown::
    /// new_text`] (or an equivalent GPUI paint step) to consume — a plain
    /// `String`, not yet wrapped in Som's own `SharedString`/GPUI entity
    /// types, since this crate has no GPUI `Context` to construct one
    /// with; the paint call site owns that step, same division of labor
    /// [`crate::rich_content_player::RichContentPlayer`] already has
    /// between "decoded pixels" (this crate) and "handed to `paint_image`"
    /// (`terminal_view`).
    rendered: String,
    /// How many contiguous bytes [`Self::rendered`] was produced from —
    /// mirrors [`crate::rich_content_player::RichContentPlayer`]'s
    /// `decoded_through` field exactly: [`refresh_or_create`] only re-runs
    /// the Lua render call when the cache's `contiguous_len` has grown
    /// past this, so a paint call between chunk arrivals is a cheap no-op.
    rendered_through: u64,
}

impl RichContentMarkdownPlayer {
    pub fn rendered(&self) -> &str { &self.rendered }
}

/// Builds (or refreshes) a [`RichContentMarkdownPlayer`] from `bytes`
/// (already sliced to `contiguous_len` by the caller) and running them
/// through [`crate::rich_content_lua_frontend::render`]. Returns
/// `existing` unchanged (`Ok(Some(_))`, cloned-free — the caller already
/// owns it) if `contiguous_len` hasn't grown since the last call, same
/// short-circuit `RichContentPlayer::refresh` uses for images.
///
/// A non-UTF-8 prefix (a chunk boundary landing mid-codepoint, since
/// markdown bytes stream in arbitrary-sized pieces same as every other
/// content type) is treated as "not enough valid content yet" rather
/// than an error — returns the previous `existing` state unchanged, the
/// same way an incomplete GIF frame doesn't fail decoding, it just
/// doesn't advance past the last complete frame.
pub fn refresh_or_create(
    bytes: &[u8],
    contiguous_len: u64,
    existing: Option<RichContentMarkdownPlayer>,
) -> anyhow::Result<Option<RichContentMarkdownPlayer>> {
    if let Some(existing) = &existing
        && existing.rendered_through >= contiguous_len
    {
        return Ok(existing_clone(existing));
    }
    if contiguous_len == 0 {
        return Ok(None);
    }
    // `bytes` can momentarily hold FEWER than `contiguous_len` bytes — a
    // late `SrvProgressState` subscriber's watermark is set from a
    // `Progress` push's numbers while the actual bytes are still in
    // flight via a separately-requested `RequestByteRange` reply (see
    // `SrvProgressState::request_whole_range_once_if_needed`'s own doc
    // comment for the exact race this covers). Treat this exactly like
    // "not enough valid content yet" — the same tolerance already
    // applied below for a mid-codepoint UTF-8 split — rather than
    // rendering (and permanently caching, via `rendered_through`) a
    // truncated or empty prefix as if it were the real, final content.
    if (bytes.len() as u64) < contiguous_len {
        return Ok(existing);
    }

    let prefix = &bytes[..(contiguous_len as usize).min(bytes.len())];
    let Ok(source) = std::str::from_utf8(prefix) else {
        return Ok(existing_clone(existing.as_ref().unwrap_or(&RichContentMarkdownPlayer { rendered: String::new(), rendered_through: 0 })));
    };
    let rendered = crate::rich_content_lua_frontend::render(source)?;
    Ok(Some(RichContentMarkdownPlayer { rendered, rendered_through: contiguous_len }))
}

fn existing_clone(existing: &RichContentMarkdownPlayer) -> Option<RichContentMarkdownPlayer> {
    Some(RichContentMarkdownPlayer { rendered: existing.rendered.clone(), rendered_through: existing.rendered_through })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_or_create_renders_a_complete_markdown_file() {
        let bytes = b"# Hello\n\nWorld.";
        let player = refresh_or_create(bytes, bytes.len() as u64, None).unwrap().unwrap();
        assert_eq!(player.rendered(), "# Hello\n\nWorld.");
    }

    #[test]
    fn refresh_or_create_skips_redecoding_when_contiguous_len_unchanged() {
        let first = refresh_or_create(b"hello", 5, None).unwrap().unwrap();
        // A DIFFERENT byte slice, but the SAME contiguous_len — if
        // refresh_or_create actually re-read it despite contiguous_len
        // being unchanged, this would show up in the result.
        let second = refresh_or_create(b"CHANGED", 5, Some(first)).unwrap().unwrap();
        assert_eq!(second.rendered(), "hello");
    }

    #[test]
    fn refresh_or_create_returns_none_for_zero_contiguous_len() {
        let result = refresh_or_create(b"hello", 0, None).unwrap();
        assert!(result.is_none());
    }
}
