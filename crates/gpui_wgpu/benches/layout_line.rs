use criterion::{Criterion, criterion_group, criterion_main};
use gpui::{FontFallbacks, FontRun, PlatformTextSystem, font, px};
use gpui_wgpu::CosmicTextSystem;
use std::borrow::Cow;

const FIRACODE_NERD_FONT: &[u8] = include_bytes!("../../../assets/fonts/FiraCodeNerdFont-Regular.ttf");

// ~4 000 chars of typical ASCII code text.
fn code_text() -> String {
    concat!(
        "    fn compute_run_spans(\n",
        "        text: &str,\n",
        "        run_offset: usize,\n",
        "        run_len: usize,\n",
        "        primary: FontId,\n",
        "        fallback_chain: &[(FontId, SharedString)],\n",
        "        covers: &impl Fn(FontId, char) -> bool,\n",
        "    ) -> SmallVec<[RunSpan; 4]> {\n",
        "        let mut spans = SmallVec::new();\n",
        "        let run_end = run_offset + run_len;\n",
        "        if run_end <= run_offset { return spans; }\n",
        "        let run_text = &text[run_offset..run_end];\n",
        "        let mut span_start = run_offset;\n",
        "        let mut span_slot: Option<usize> = None;\n",
        "        for (ch_idx, ch) in run_text.char_indices() {\n",
        "            let abs = run_offset + ch_idx;\n",
        "            let next = pick_covering_slot(ch, span_slot, primary, fallback_chain, covers);\n",
        "            if next == span_slot { continue; }\n",
        "            if abs > span_start {\n",
        "                spans.push(RunSpan { start: span_start, end: abs, slot: span_slot });\n",
        "            }\n",
        "            span_start = abs;\n",
        "            span_slot = next;\n",
        "        }\n",
        "        spans\n",
        "    }\n",
    )
    .repeat(8) // ~3 800 chars
}

fn bench_layout_line(c: &mut Criterion) {
    // Uses the system font database (unlike `new_without_system_fonts`) so
    // "Segoe UI" — used below purely as a second, distinct fallback font to
    // measure fallback-chain overhead against — resolves to a real font
    // without needing its own bundled asset.
    let system = CosmicTextSystem::new("FiraCode Nerd Font");
    system.add_fonts(vec![Cow::Borrowed(FIRACODE_NERD_FONT)]).unwrap();

    let font_id_no_fallback = system.font_id(&font("FiraCode Nerd Font")).unwrap();

    let font_id_with_fallback = {
        let mut f = font("FiraCode Nerd Font");
        f.fallbacks = Some(FontFallbacks::from_fonts(vec!["Segoe UI".to_string()]));
        system.font_id(&f).unwrap()
    };

    let text = code_text();

    let runs_no_fallback = vec![FontRun {
        len: text.len(),
        font_id: font_id_no_fallback,
    }];
    let runs_with_fallback = vec![FontRun {
        len: text.len(),
        font_id: font_id_with_fallback,
    }];

    let mut group = c.benchmark_group("layout_line");

    group.bench_function("no_fallback", |b| {
        b.iter(|| system.layout_line(&text, px(14.0), &runs_no_fallback))
    });

    group.bench_function("with_fallback_ascii", |b| {
        b.iter(|| system.layout_line(&text, px(14.0), &runs_with_fallback))
    });

    group.finish();
}

criterion_group!(benches, bench_layout_line);
criterion_main!(benches);
