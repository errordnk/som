//! Lua *backend* runtime — `som-srv`'s own source of `PutChunk`s, not just
//! a relay for chunks an external client already had. See `SRP_LUA.md`'s
//! "Phase 1" section in the Som repository for the full design context;
//! this file is that phase's entire implementation.
//!
//! Deliberately minimal: a fresh `mlua::Lua` per invocation (see
//! `SRP_LUA.md`'s "Open questions" for why persistent-VM lifecycle is a
//! later decision, not this one), no Rust functions registered into it at
//! all yet — a Phase 1 script is a pure `return "..."` expression with no
//! filesystem/network/DB access, proving the transport (script source in,
//! markdown chunked out through the exact same path `PutChunk` already
//! uses) before Phase 2/3 grow what a script can actually DO.

use crate::srv_cache::SrvCache;
use som_srv::protocol::{ContentMetadata, ContentType};

/// Same chunk size every other `PutChunk` sender in this project uses
/// (`somcat::CHUNK_SIZE`, the yazi driver's own `CHUNK_SIZE`) — no reason
/// for the Lua-originated path to pick a different number.
const CHUNK_SIZE: usize = 65536;

/// Runs `script_source` to completion in a fresh `mlua::Lua` VM, takes its
/// single string return value as markdown source, and pushes it through
/// `cache.put_chunk` in `CHUNK_SIZE` pieces — the exact same call an
/// external `PutChunk` sender's handler already makes (see
/// `server::handle_srv_request`'s `SrvRequest::PutChunk` arm), just driven
/// by this function's own loop instead of a stream of wire messages.
///
/// `content_type`/`metadata` are always `ContentType::Markdown`/
/// `ContentMetadata::Markdown` for now — see `ContentMetadata::Markdown`'s
/// own doc comment for why that variant carries no geometric/format
/// fields to fill in even if this function wanted to.
pub fn run_and_stream(cache: &SrvCache, session_id: u32, file_id: u32, script_source: &str) -> anyhow::Result<()> {
    let markdown = run_script(script_source)?;
    let bytes = markdown.as_bytes();
    let total_size = bytes.len() as u64;
    let cache_dir = SrvCache::default_cache_dir();

    for (index, chunk) in bytes.chunks(CHUNK_SIZE).enumerate() {
        let offset = (index * CHUNK_SIZE) as u64;
        cache.put_chunk(&cache_dir, session_id, file_id, offset, chunk, total_size, ContentType::Markdown, ContentMetadata::Markdown)?;
    }
    // An empty script result still needs ONE put_chunk call (offset 0,
    // zero-length data) so `total_size` reaches som-srv's cache and Som's
    // own `contiguous_len` watermark can reach it — `bytes.chunks(...)`
    // yields nothing at all for an empty slice, unlike every other
    // length, so this is the one case the loop above doesn't cover.
    if bytes.is_empty() {
        cache.put_chunk(&cache_dir, session_id, file_id, 0, &[], 0, ContentType::Markdown, ContentMetadata::Markdown)?;
    }
    Ok(())
}

/// Standard libraries a Phase 1 script gets — deliberately NOT
/// `Lua::new()`'s own default. Confirmed by direct testing (`run_script_
/// has_no_io_library_available`, below) that `mlua::Lua::new()` is
/// `Lua::new_with(StdLib::ALL_SAFE, ...)` under the hood, and `StdLib::
/// ALL_SAFE` — per mlua's own `stdlib.rs` — includes `IO`, having nothing
/// to do with filesystem access despite the name (mlua's "safe" only
/// means "doesn't corrupt the VM," not "no side effects on the host") —
/// an earlier version of this module assumed `Lua::new()` already omitted
/// `io`/`os`, matching an incorrect read of how yazi's own `mlua` setup
/// behaves; that assumption was wrong and this comment/the accompanying
/// test are the correction. `TABLE | STRING | MATH | UTF8` gives a script
/// pure computation (string formatting, table manipulation, arithmetic)
/// with no way to touch the filesystem, network, environment, or process
/// — `IO`/`OS`/`PACKAGE` (which can `require` arbitrary modules) are all
/// excluded on purpose. `COROUTINE` is included since it can't reach
/// outside the VM either and script authors may reasonably want it.
fn phase1_stdlib() -> mlua::StdLib {
    mlua::StdLib::TABLE | mlua::StdLib::STRING | mlua::StdLib::MATH | mlua::StdLib::UTF8 | mlua::StdLib::COROUTINE
}

/// Executes `script_source` and returns its single string return value.
/// Uses `PHASE1_STDLIB` (see its own doc comment for why NOT `Lua::
/// new()`'s default) — no Rust functions are registered into `lua`'s
/// globals at all in Phase 1 either, so a script has nothing to call
/// outside the stdlib subset itself.
fn run_script(script_source: &str) -> anyhow::Result<String> {
    let lua = mlua::Lua::new_with(phase1_stdlib(), mlua::LuaOptions::default())?;
    let value: mlua::Value = lua.load(script_source).eval()?;
    match value {
        mlua::Value::String(s) => Ok(s.to_str()?.to_string()),
        other => anyhow::bail!("Lua script must return a string, got {}", other.type_name()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_script_returns_the_script_s_string_result() {
        let script = "return \"# Hello\\n\\nThis is markdown.\"";
        let result = run_script(script).unwrap();
        assert_eq!(result, "# Hello\n\nThis is markdown.");
    }

    #[test]
    fn run_script_rejects_a_non_string_return_value() {
        let err = run_script("return 42").unwrap_err();
        assert!(err.to_string().contains("must return a string"), "unexpected error: {err}");
    }

    #[test]
    fn run_script_propagates_a_lua_syntax_error() {
        let err = run_script("this is not valid lua").unwrap_err();
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn run_script_has_no_io_library_available() {
        // Confirms the "no explicit sandbox flags needed" claim in this
        // module's own doc comment — `io` must be genuinely absent from a
        // plain `Lua::new()`, not just unused by these tests.
        let err = run_script(r#"io.open("whatever"); return "unreachable""#).unwrap_err();
        assert!(err.to_string().contains("io"), "expected an error referencing the missing `io` global, got: {err}");
    }
}
