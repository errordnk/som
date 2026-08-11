//! Unicode-placeholder cell encoding, originally specified by the Kitty
//! terminal graphics protocol's "Unicode placeholders" mode (`U=1`,
//! <https://sw.kovidgoyal.net/kitty/graphics-protocol/#unicode-placeholders>)
//! and reused as-is by Som's own rich-content protocol
//! (`crate::rich_content_transport`/`Terminal::print_rich_content_placeholder_grid`).
//! This mode doesn't anchor an image to a pixel/cell position tracked
//! separately from the grid — the image's position/size IS encoded
//! directly into ordinary text cells, written as a special placeholder
//! character (`U+10EEEE`) whose foreground color holds an id and whose
//! combining diacritics (from a fixed 297-entry Unicode table, see
//! [`ROWCOLUMN_DIACRITICS`]) encode which row/column of the placement each
//! individual cell represents.
//!
//! This module only handles the ENCODING/DECODING of that representation —
//! it doesn't touch the terminal grid itself (writing placeholder cells is
//! `Term`'s job, same as writing any other character).

/// The base placeholder character every unicode-placeholder-mode cell uses.
/// A bare `U+10EEEE` with no diacritics represents row 0, column 0 of
/// whichever image its foreground color identifies.
pub const PLACEHOLDER_CHAR: char = '\u{10EEEE}';

/// The 297 combining diacritics used to encode row/column/extended-id
/// values, in the exact order Kitty's own reference table
/// (`rowcolumn-diacritics.txt`, derived from Unicode 6.0.0's class-230
/// combining characters) lists them — index into this array IS the value
/// being encoded, so reordering these breaks compatibility with every other
/// terminal/program implementing this spec.
///
/// Downloaded directly from
/// <https://sw.kovidgoyal.net/kitty/_downloads/f0a0de9ec8d9ff4456206db8e0814937/rowcolumn-diacritics.txt>
/// and mechanically extracted (not hand-transcribed — a table this size is
/// exactly the kind of thing a single wrong character silently breaks).
pub const ROWCOLUMN_DIACRITICS: [char; 297] = [
    '\u{0305}', '\u{030D}', '\u{030E}', '\u{0310}', '\u{0312}', '\u{033D}', '\u{033E}',
    '\u{033F}', '\u{0346}', '\u{034A}', '\u{034B}', '\u{034C}', '\u{0350}', '\u{0351}',
    '\u{0352}', '\u{0357}', '\u{035B}', '\u{0363}', '\u{0364}', '\u{0365}', '\u{0366}',
    '\u{0367}', '\u{0368}', '\u{0369}', '\u{036A}', '\u{036B}', '\u{036C}', '\u{036D}',
    '\u{036E}', '\u{036F}', '\u{0483}', '\u{0484}', '\u{0485}', '\u{0486}', '\u{0487}',
    '\u{0592}', '\u{0593}', '\u{0594}', '\u{0595}', '\u{0597}', '\u{0598}', '\u{0599}',
    '\u{059C}', '\u{059D}', '\u{059E}', '\u{059F}', '\u{05A0}', '\u{05A1}', '\u{05A8}',
    '\u{05A9}', '\u{05AB}', '\u{05AC}', '\u{05AF}', '\u{05C4}', '\u{0610}', '\u{0611}',
    '\u{0612}', '\u{0613}', '\u{0614}', '\u{0615}', '\u{0616}', '\u{0617}', '\u{0657}',
    '\u{0658}', '\u{0659}', '\u{065A}', '\u{065B}', '\u{065D}', '\u{065E}', '\u{06D6}',
    '\u{06D7}', '\u{06D8}', '\u{06D9}', '\u{06DA}', '\u{06DB}', '\u{06DC}', '\u{06DF}',
    '\u{06E0}', '\u{06E1}', '\u{06E2}', '\u{06E4}', '\u{06E7}', '\u{06E8}', '\u{06EB}',
    '\u{06EC}', '\u{0730}', '\u{0732}', '\u{0733}', '\u{0735}', '\u{0736}', '\u{073A}',
    '\u{073D}', '\u{073F}', '\u{0740}', '\u{0741}', '\u{0743}', '\u{0745}', '\u{0747}',
    '\u{0749}', '\u{074A}', '\u{07EB}', '\u{07EC}', '\u{07ED}', '\u{07EE}', '\u{07EF}',
    '\u{07F0}', '\u{07F1}', '\u{07F3}', '\u{0816}', '\u{0817}', '\u{0818}', '\u{0819}',
    '\u{081B}', '\u{081C}', '\u{081D}', '\u{081E}', '\u{081F}', '\u{0820}', '\u{0821}',
    '\u{0822}', '\u{0823}', '\u{0825}', '\u{0826}', '\u{0827}', '\u{0829}', '\u{082A}',
    '\u{082B}', '\u{082C}', '\u{082D}', '\u{0951}', '\u{0953}', '\u{0954}', '\u{0F82}',
    '\u{0F83}', '\u{0F86}', '\u{0F87}', '\u{135D}', '\u{135E}', '\u{135F}', '\u{17DD}',
    '\u{193A}', '\u{1A17}', '\u{1A75}', '\u{1A76}', '\u{1A77}', '\u{1A78}', '\u{1A79}',
    '\u{1A7A}', '\u{1A7B}', '\u{1A7C}', '\u{1B6B}', '\u{1B6D}', '\u{1B6E}', '\u{1B6F}',
    '\u{1B70}', '\u{1B71}', '\u{1B72}', '\u{1B73}', '\u{1CD0}', '\u{1CD1}', '\u{1CD2}',
    '\u{1CDA}', '\u{1CDB}', '\u{1CE0}', '\u{1DC0}', '\u{1DC1}', '\u{1DC3}', '\u{1DC4}',
    '\u{1DC5}', '\u{1DC6}', '\u{1DC7}', '\u{1DC8}', '\u{1DC9}', '\u{1DCB}', '\u{1DCC}',
    '\u{1DD1}', '\u{1DD2}', '\u{1DD3}', '\u{1DD4}', '\u{1DD5}', '\u{1DD6}', '\u{1DD7}',
    '\u{1DD8}', '\u{1DD9}', '\u{1DDA}', '\u{1DDB}', '\u{1DDC}', '\u{1DDD}', '\u{1DDE}',
    '\u{1DDF}', '\u{1DE0}', '\u{1DE1}', '\u{1DE2}', '\u{1DE3}', '\u{1DE4}', '\u{1DE5}',
    '\u{1DE6}', '\u{1DFE}', '\u{20D0}', '\u{20D1}', '\u{20D4}', '\u{20D5}', '\u{20D6}',
    '\u{20D7}', '\u{20DB}', '\u{20DC}', '\u{20E1}', '\u{20E7}', '\u{20E9}', '\u{20F0}',
    '\u{2CEF}', '\u{2CF0}', '\u{2CF1}', '\u{2DE0}', '\u{2DE1}', '\u{2DE2}', '\u{2DE3}',
    '\u{2DE4}', '\u{2DE5}', '\u{2DE6}', '\u{2DE7}', '\u{2DE8}', '\u{2DE9}', '\u{2DEA}',
    '\u{2DEB}', '\u{2DEC}', '\u{2DED}', '\u{2DEE}', '\u{2DEF}', '\u{2DF0}', '\u{2DF1}',
    '\u{2DF2}', '\u{2DF3}', '\u{2DF4}', '\u{2DF5}', '\u{2DF6}', '\u{2DF7}', '\u{2DF8}',
    '\u{2DF9}', '\u{2DFA}', '\u{2DFB}', '\u{2DFC}', '\u{2DFD}', '\u{2DFE}', '\u{2DFF}',
    '\u{A66F}', '\u{A67C}', '\u{A67D}', '\u{A6F0}', '\u{A6F1}', '\u{A8E0}', '\u{A8E1}',
    '\u{A8E2}', '\u{A8E3}', '\u{A8E4}', '\u{A8E5}', '\u{A8E6}', '\u{A8E7}', '\u{A8E8}',
    '\u{A8E9}', '\u{A8EA}', '\u{A8EB}', '\u{A8EC}', '\u{A8ED}', '\u{A8EE}', '\u{A8EF}',
    '\u{A8F0}', '\u{A8F1}', '\u{AAB0}', '\u{AAB2}', '\u{AAB3}', '\u{AAB7}', '\u{AAB8}',
    '\u{AABE}', '\u{AABF}', '\u{AAC1}', '\u{FE20}', '\u{FE21}', '\u{FE22}', '\u{FE23}',
    '\u{FE24}', '\u{FE25}', '\u{FE26}', '\u{10A0F}', '\u{10A38}', '\u{1D185}', '\u{1D186}',
    '\u{1D187}', '\u{1D188}', '\u{1D189}', '\u{1D1AA}', '\u{1D1AB}', '\u{1D1AC}', '\u{1D1AD}',
    '\u{1D242}', '\u{1D243}', '\u{1D244}',
];

/// Encode a (row, column) offset within a placement as a diacritic pair
/// appended to [`PLACEHOLDER_CHAR`]: row first, then column, matching the
/// order every other implementation of this spec uses (Kitty itself, plus
/// the terminals/multiplexers that copied it — deviating here would make
/// Som's own output unreadable by anyone else's parser, even though
/// decoding is symmetric either way internally).
///
/// `row`/`column` are relative to the placement's own top-left corner (both
/// start at 0), NOT absolute grid coordinates — this only encodes "which
/// cell of this multi-cell image is this", the placement's overall grid
/// position is wherever the placeholder text itself was written, same as
/// any other character.
///
/// Returns `None` if `row` or `column` is too large to represent (the table
/// only has 297 entries — values beyond that aren't encodable at all per
/// spec, not a bug in this implementation).
pub fn encode_cell(row: u32, column: u32) -> Option<[char; 3]> {
    let row_diacritic = *ROWCOLUMN_DIACRITICS.get(row as usize)?;
    let col_diacritic = *ROWCOLUMN_DIACRITICS.get(column as usize)?;
    Some([PLACEHOLDER_CHAR, row_diacritic, col_diacritic])
}

/// Decode a placeholder cell's diacritics back into (row, column).
///
/// `diacritics` should be the combining characters that followed a
/// [`PLACEHOLDER_CHAR`] cell in the grid (in row, column order — see
/// [`encode_cell`]) — typically just the first 1-2 entries a real terminal
/// grid attaches to that cell as zero-width combining marks. Returns
/// `(0, 0)` for an empty slice (a bare placeholder with no diacritics IS
/// row 0, column 0 per spec, not malformed input) and `None` only if a
/// diacritic present doesn't match any table entry at all (not part of
/// this encoding — genuinely corrupt/foreign data).
pub fn decode_cell(diacritics: &[char]) -> Option<(u32, u32)> {
    let row = match diacritics.first() {
        None => 0,
        Some(&c) => diacritic_index(c)? as u32,
    };
    let column = match diacritics.get(1) {
        None => 0,
        Some(&c) => diacritic_index(c)? as u32,
    };
    Some((row, column))
}

fn diacritic_index(c: char) -> Option<usize> {
    ROWCOLUMN_DIACRITICS.iter().position(|&d| d == c)
}

/// A decoded reference to one cell's worth of an image, extracted from a
/// grid cell that used the Unicode placeholder encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaceholderCell {
    pub image_id: u32,
    /// `None` when no distinct underline color was set on the cell — per
    /// spec this means placement id 0, i.e. "the only/default placement".
    pub placement_id: u32,
    pub row: u32,
    pub column: u32,
}

/// Decode one placeholder cell, given its character, foreground color (as
/// 24-bit RGB — see below), underline color if the cell has one set
/// distinctly from its foreground, and the combining diacritics attached to
/// it (alacritty's `Cell::zerowidth()`).
///
/// Returns `None` if `c` isn't [`PLACEHOLDER_CHAR`] at all (not an error —
/// callers scan every cell in the visible grid and most simply aren't
/// placeholders) or if the diacritics present don't decode to a valid
/// row/column (genuinely malformed placeholder data).
///
/// Colors are taken as plain `(u8, u8, u8)` rather than
/// `alacritty_terminal::vte::ansi::Color`/`Rgb` so this module stays free of
/// an `alacritty_terminal` dependency — the caller (which already depends
/// on `alacritty_terminal` to read the grid in the first place) is
/// responsible for resolving a cell's `Color` enum (`Named`/`Indexed`/
/// `Spec`) down to concrete RGB bytes before calling this function;
/// `Named`/`Indexed` colors resolve through the terminal's active color
/// palette, which this module has no access to and no need to know about.
///
/// Per spec, the image id is the foreground color's 24-bit RGB value
/// interpreted as a big-endian `u32` (`r << 16 | g << 8 | b`) when using
/// true-color id encoding — the 8-bit/256-color id encoding variant (id fits
/// in a single palette index, extended via a 3rd diacritic byte) is not
/// handled by this function; see `KITTY_GRAPHICS_PLAN.md` Stage 4 for
/// whether Som needs it (yazi/Kitty itself default to the true-color
/// variant).
pub fn decode_placeholder_cell(
    c: char,
    fg_rgb: (u8, u8, u8),
    underline_rgb: Option<(u8, u8, u8)>,
    diacritics: &[char],
) -> Option<PlaceholderCell> {
    if c != PLACEHOLDER_CHAR {
        return None;
    }
    let image_id = rgb_to_id(fg_rgb);
    let placement_id = underline_rgb.map(rgb_to_id).unwrap_or(0);
    let (row, column) = decode_cell(diacritics)?;
    Some(PlaceholderCell { image_id, placement_id, row, column })
}

fn rgb_to_id((r, g, b): (u8, u8, u8)) -> u32 {
    (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_has_exactly_297_entries() {
        // The spec's own reference file has exactly this many rows —
        // a wrong count here would silently shift every index above it.
        assert_eq!(ROWCOLUMN_DIACRITICS.len(), 297);
    }

    #[test]
    fn table_first_and_last_entries_match_the_reference_file() {
        // Spot-check against the actual downloaded reference file (see
        // this module's ROWCOLUMN_DIACRITICS doc comment) rather than
        // trusting the full 297-entry transcription blindly.
        assert_eq!(ROWCOLUMN_DIACRITICS[0], '\u{0305}');
        assert_eq!(ROWCOLUMN_DIACRITICS[1], '\u{030D}');
        assert_eq!(ROWCOLUMN_DIACRITICS[2], '\u{030E}');
        assert_eq!(ROWCOLUMN_DIACRITICS[296], '\u{1D244}');
        assert_eq!(ROWCOLUMN_DIACRITICS[295], '\u{1D243}');
        assert_eq!(ROWCOLUMN_DIACRITICS[294], '\u{1D242}');
    }

    #[test]
    fn table_has_no_duplicate_entries() {
        // Every entry must be unique, or decode_cell's index lookup would
        // be ambiguous for whichever duplicate exists.
        let mut sorted: Vec<char> = ROWCOLUMN_DIACRITICS.to_vec();
        sorted.sort_unstable();
        let mut deduped = sorted.clone();
        deduped.dedup();
        assert_eq!(sorted.len(), deduped.len(), "table must have no duplicate diacritics");
    }

    #[test]
    fn encode_row0_col0_matches_spec_example() {
        // Per the spec's own worked example: row 0 uses U+0305, and a bare
        // placeholder (no diacritics at all) also means row 0, col 0.
        let encoded = encode_cell(0, 0).unwrap();
        assert_eq!(encoded[0], PLACEHOLDER_CHAR);
        assert_eq!(encoded[1], '\u{0305}');
        assert_eq!(encoded[2], '\u{0305}');
    }

    #[test]
    fn encode_col1_matches_spec_example() {
        // Per the spec's own worked example: column 1 uses U+030D.
        let encoded = encode_cell(0, 1).unwrap();
        assert_eq!(encoded[2], '\u{030D}');
    }

    #[test]
    fn encode_decode_roundtrip_for_every_valid_index_pair() {
        // Exhaustive: every (row, column) combination the table can
        // represent must decode back to exactly what was encoded.
        for row in 0..297u32 {
            for column in [0, 1, 100, 296] {
                let encoded = encode_cell(row, column).unwrap();
                let decoded = decode_cell(&encoded[1..]).unwrap();
                assert_eq!(decoded, (row, column), "roundtrip failed for ({row}, {column})");
            }
        }
    }

    #[test]
    fn encode_out_of_range_returns_none() {
        assert!(encode_cell(297, 0).is_none());
        assert!(encode_cell(0, 297).is_none());
        assert!(encode_cell(u32::MAX, u32::MAX).is_none());
    }

    #[test]
    fn decode_empty_diacritics_is_row0_col0() {
        // A bare placeholder character with no combining marks at all is
        // valid per spec, meaning (0, 0) — not an error.
        assert_eq!(decode_cell(&[]), Some((0, 0)));
    }

    #[test]
    fn decode_single_diacritic_defaults_column_to_zero() {
        // Only a row diacritic present (no column one) — column defaults
        // to 0, same reasoning as the empty case.
        assert_eq!(decode_cell(&['\u{030D}']), Some((1, 0)));
    }

    #[test]
    fn decode_unknown_character_returns_none() {
        // A combining character that isn't in the table at all — genuinely
        // not this encoding, not just an edge case of it.
        assert_eq!(decode_cell(&['\u{0301}']), None); // acute accent, not in the table
    }

    #[test]
    fn decode_placeholder_cell_non_placeholder_char_is_none() {
        // Scanning the grid hits plenty of ordinary text cells — must not
        // misinterpret them as placeholders just because they happen to
        // have some fg color and diacritics-shaped input.
        assert_eq!(decode_placeholder_cell('a', (1, 2, 3), None, &[]), None);
    }

    #[test]
    fn decode_placeholder_cell_extracts_image_id_from_fg_rgb() {
        let decoded =
            decode_placeholder_cell(PLACEHOLDER_CHAR, (0x12, 0x34, 0x56), None, &[]).unwrap();
        assert_eq!(decoded.image_id, 0x00123456);
        assert_eq!(decoded.placement_id, 0);
        assert_eq!(decoded.row, 0);
        assert_eq!(decoded.column, 0);
    }

    #[test]
    fn decode_placeholder_cell_extracts_placement_id_from_underline_rgb() {
        let decoded = decode_placeholder_cell(
            PLACEHOLDER_CHAR,
            (0, 0, 1),
            Some((0, 0, 42)),
            &[],
        )
        .unwrap();
        assert_eq!(decoded.image_id, 1);
        assert_eq!(decoded.placement_id, 42);
    }

    #[test]
    fn decode_placeholder_cell_extracts_row_and_column_from_diacritics() {
        let diacritics = [ROWCOLUMN_DIACRITICS[3], ROWCOLUMN_DIACRITICS[5]];
        let decoded =
            decode_placeholder_cell(PLACEHOLDER_CHAR, (0, 0, 0), None, &diacritics).unwrap();
        assert_eq!(decoded.row, 3);
        assert_eq!(decoded.column, 5);
    }

    #[test]
    fn decode_placeholder_cell_invalid_diacritic_returns_none() {
        let decoded =
            decode_placeholder_cell(PLACEHOLDER_CHAR, (0, 0, 0), None, &['\u{0301}']);
        assert_eq!(decoded, None);
    }
}
