//! Lua *frontend* runtime — see `SRP_LUA.md`'s "Phase 1" section for the
//! full design context. Where `som_srv::lua` (in `crates/som_srv`) is the
//! backend that ORIGINATES markdown content, this module is what Som
//! itself runs to decide how to interpret/shape a `ContentType::Markdown`
//! payload BEFORE handing it to [`crate::rich_content_markdown_player`]
//! for painting — the frontend half of the backend/frontend split
//! `SRP_LUA.md` describes.
//!
//! Phase 1 keeps this deliberately trivial: `render(source) -> source`,
//! the identity function, with no Rust bindings registered into the VM at
//! all. This exists now (rather than skipping straight to `Markdown::
//! new_text` with no Lua involved) so the render call site — where a
//! future script actually gets to intercept and reshape content — is
//! already wired and proven, ahead of Phase 2 growing what that script
//! can actually control.

use mlua::{Lua, LuaOptions, StdLib};

/// Same allow-list `som_srv::lua::phase1_stdlib` uses on the backend side
/// — see that function's own doc comment for why NOT `Lua::new()`'s
/// default (`StdLib::ALL_SAFE` includes `io`, confirmed by direct test,
/// not the "safe" a casual reading of the name suggests). The frontend
/// VM has exactly the same "pure computation only" requirement the
/// backend one does — Som's own process already has full OS access, but
/// nothing about that means a markdown-rendering script run inside it
/// should.
fn phase1_stdlib() -> StdLib { StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8 | StdLib::COROUTINE }

/// Runs the default Phase 1 frontend script (`return function(source)
/// return source end`) against `source` and returns its result. A
/// dedicated VM per call, matching the backend's own fresh-per-invocation
/// lifecycle (see `SRP_LUA.md`'s "Open questions" section — the two are
/// the same underlying decision, revisited together once Phase 3 needs a
/// persistent VM for held-open resources).
///
/// Phase 1 has no per-placement script selection yet — every `Content
/// Type::Markdown` placement runs through this exact same identity
/// transform. That's the whole point of Phase 1: prove the call site
/// works before anything reaches through it to actually change the
/// output.
pub fn render(source: &str) -> anyhow::Result<String> {
    let lua = Lua::new_with(phase1_stdlib(), LuaOptions::default())?;
    let render_fn: mlua::Function = lua.load("return function(source) return source end").eval()?;
    let result: mlua::Value = render_fn.call(source)?;
    match result {
        mlua::Value::String(s) => Ok(s.to_str()?.to_string()),
        other => anyhow::bail!("frontend render() must return a string, got {}", other.type_name()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_is_the_identity_function_in_phase_1() {
        let result = render("# Hello\n\nThis is markdown.").unwrap();
        assert_eq!(result, "# Hello\n\nThis is markdown.");
    }

    #[test]
    fn render_has_no_io_library_available() {
        // Same live-tested sandboxing guarantee as the backend side —
        // see `phase1_stdlib`'s own doc comment for why this can't be
        // assumed from `Lua::new()`'s default.
        let lua = Lua::new_with(phase1_stdlib(), LuaOptions::default()).unwrap();
        let err = lua.load("io.open(\"whatever\")").exec().unwrap_err();
        assert!(err.to_string().contains("io"), "expected an error referencing the missing `io` global, got: {err}");
    }
}
