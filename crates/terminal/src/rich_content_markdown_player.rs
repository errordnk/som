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

/// Builds (or refreshes) a [`RichContentMarkdownPlayer`] from `path`,
/// reading exactly `contiguous_len` bytes (never more — bytes past that
/// watermark haven't necessarily arrived yet, same gap-tolerance every
/// other rich-content decode path in this crate already respects) and
/// running them through [`crate::rich_content_lua_frontend::render`].
/// Returns `existing` unchanged (`Ok(Some(_))`, cloned-free — the caller
/// already owns it) if `contiguous_len` hasn't grown since the last call,
/// same short-circuit `RichContentPlayer::refresh` uses for images.
///
/// A non-UTF-8 prefix (a chunk boundary landing mid-codepoint, since
/// markdown bytes stream in arbitrary-sized pieces same as every other
/// content type) is treated as "not enough valid content yet" rather
/// than an error — returns the previous `existing` state unchanged, the
/// same way an incomplete GIF frame doesn't fail decoding, it just
/// doesn't advance past the last complete frame.
pub fn refresh_or_create(
    path: &std::path::Path,
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

    let bytes = read_prefix(path, contiguous_len)?;
    let Ok(source) = std::str::from_utf8(&bytes) else {
        return Ok(existing_clone(existing.as_ref().unwrap_or(&RichContentMarkdownPlayer { rendered: String::new(), rendered_through: 0 })));
    };
    let rendered = crate::rich_content_lua_frontend::render(source)?;
    Ok(Some(RichContentMarkdownPlayer { rendered, rendered_through: contiguous_len }))
}

fn existing_clone(existing: &RichContentMarkdownPlayer) -> Option<RichContentMarkdownPlayer> {
    Some(RichContentMarkdownPlayer { rendered: existing.rendered.clone(), rendered_through: existing.rendered_through })
}

fn read_prefix(path: &std::path::Path, len: u64) -> anyhow::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; len as usize];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_or_create_renders_a_complete_markdown_file() {
        let dir = std::env::temp_dir().join(format!("som_markdown_player_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.md");
        std::fs::write(&path, "# Hello\n\nWorld.").unwrap();

        let player = refresh_or_create(&path, 15, None).unwrap().unwrap();
        assert_eq!(player.rendered(), "# Hello\n\nWorld.");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refresh_or_create_skips_redecoding_when_contiguous_len_unchanged() {
        let dir = std::env::temp_dir().join(format!("som_markdown_player_test2_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.md");
        std::fs::write(&path, "hello").unwrap();

        let first = refresh_or_create(&path, 5, None).unwrap().unwrap();
        // Overwrite the file on disk — if refresh_or_create actually
        // re-read it despite contiguous_len being unchanged, this would
        // show up in the result.
        std::fs::write(&path, "CHANGED").unwrap();
        let second = refresh_or_create(&path, 5, Some(first)).unwrap().unwrap();
        assert_eq!(second.rendered(), "hello");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refresh_or_create_returns_none_for_zero_contiguous_len() {
        let dir = std::env::temp_dir().join(format!("som_markdown_player_test3_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.md");
        std::fs::write(&path, "hello").unwrap();

        let result = refresh_or_create(&path, 0, None).unwrap();
        assert!(result.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
