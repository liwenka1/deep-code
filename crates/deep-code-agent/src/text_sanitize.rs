//! Neutralizing model- and repo-controlled text before it reaches a terminal.
//!
//! # Why this lives in the agent crate
//!
//! It used to live in `deep-code-tui/src/ui/render.rs`, next to its first
//! caller. `deep-code-runtime` does not depend on `deep-code-tui`, so the
//! `serve` warning loops could not reach it even in principle — and the other
//! naked surfaces (`deepcode session list`, the headless `-p` stderr lines,
//! `deepcode doctor`, `deepcode github status`) were all outside the module
//! that owned the rule. Nine separate commits sanitized one surface at a time
//! and each time missed a sibling, which is not nine oversights: a defense
//! whose reachability is narrower than its threat surface produces exactly that
//! pattern. Putting it in the crate every other crate already depends on is the
//! structural half of the fix.
//!
//! # The threat
//!
//! Tool names, file names, paths, session previews, config values and assistant
//! output are attacker-influenced: a repository ships them and the model
//! chooses them. Reaching a terminal intact, `\x1b[8m` (SGR conceal) hides
//! everything drawn after it in the same frame — including an approval panel a
//! human is reading to make an authorization decision — and `\x1b[12;3H`
//! repositions the cursor to paint a convincing fake line. The invisible
//! Unicode families do the same job without an escape byte.

/// Invisible code points a model must not be able to place on screen: the
/// full Unicode `Bidi_Control` set (ALM, LRM/RLM, LRE/RLE/PDF/LRO/RLO, the
/// isolates), the zero-width spacing/format characters (ZWSP, SHY, WJ,
/// U+FEFF), the interlinear-annotation trio, the line/paragraph separators,
/// and all FOUR Hangul fillers — U+FFA0 was missing while the doc claimed the
/// set. Every one is invisible, carries reordering or padding potential, and
/// has no legitimate role in a terminal transcript.
///
/// Deliberately ABSENT: ZWNJ/ZWJ and the variation selectors. Those join or
/// restyle real graphemes (emoji, Persian) and deleting them would corrupt
/// legitimate text; they stay safe by a measured invariant instead — each
/// rides inside the preceding grapheme cluster's cell, never a column of its
/// own. Also absent: NBSP and the other fixed-width spaces, which are honest
/// about the columns they take.
///
/// Contiguous families live in [`INVISIBLE_RANGES`] rather than here. Note that
/// the two tables are not partitioned by family — `BIDI_AND_ZERO_WIDTH` holds
/// the isolated members of several families whose contiguous neighbours are in
/// the ranges table (LRM/RLM here, LRE–RLO there; the Kaithi marks here, the
/// rest of the Prepended_Concatenation_Mark family there). Read the two
/// together, and rely on `neutralize_strips_every_invisible_code_point` rather
/// than on either table looking complete on its own.
const BIDI_AND_ZERO_WIDTH: [char; 18] = [
    '\u{00ad}',  // SHY
    '\u{061c}',  // ALM
    '\u{06dd}',  // ARABIC END OF AYAH (prepended mark, measures 1 column)
    '\u{070f}',  // SYRIAC ABBREVIATION MARK
    '\u{08e2}',  // ARABIC DISPUTED END OF AYAH
    '\u{115f}',  // HANGUL CHOSEONG FILLER (invisible, measures 2 columns)
    '\u{1160}',  // HANGUL JUNGSEONG FILLER
    '\u{180e}',  // MONGOLIAN VOWEL SEPARATOR
    '\u{200b}',  // ZWSP
    '\u{200e}',  // LRM
    '\u{200f}',  // RLM
    '\u{2028}',  // LINE SEPARATOR
    '\u{2029}',  // PARAGRAPH SEPARATOR
    '\u{3164}',  // HANGUL FILLER
    '\u{feff}',  // ZWNBSP/BOM
    '\u{ffa0}',  // HALFWIDTH HANGUL FILLER
    '\u{110bd}', // KAITHI NUMBER SIGN
    '\u{110cd}', // KAITHI NUMBER SIGN ABOVE
];

/// The contiguous half, kept as ranges because enumerating them would drown
/// the code points a reader actually needs to see.
///
/// Two of these close gaps that the "stop renting ratatui's behavior" pass
/// left open, in the two different shapes such a gap comes in:
///
/// * U+2028/U+2029 were a REAL hole. They are `Zl`/`Zp`, so `char::is_control`
///   says no; they measure one column, so ratatui does NOT drop them; and they
///   are line/paragraph separators, so an emulator that honours them shifts
///   the frame by a row — after which ratatui's diff-based redraw paints every
///   later frame at the wrong offset. That is an approval-panel repaint
///   primitive reachable through a pipeline whose stated model is "wrapping
///   already consumed the real newlines".
/// * The width-0 families (invisible operators, deprecated format controls,
///   musical/shorthand format controls, U+FFA0, U+180E) were SAFE — but safe
///   only because ratatui skips zero-width symbols, which is exactly the
///   undocumented, one-bump-away behavior this defense was rewritten to stop
///   leaning on. Same class, same fix: own it here.
///
/// The Egyptian block was filed under the second bullet and did not belong
/// there: all sixteen measure ONE column and take a cell of their own, so they
/// were a real hole like U+2028, never a lease. Nothing to change in the table,
/// but the reason had to stop being wrong.
///
/// The Prepended_Concatenation_Mark family is here for the first bullet's
/// reason. It clusters with the FOLLOWING character, and ratatui segments
/// graphemes per span — so at a span tail, a line tail, or before a wrap, each
/// becomes its own cluster and claims its own cell. U+0600–U+0604 and U+06DD
/// measure 1 column; the rest measure 0 and were another lease. Half the family
/// (the two Kaithi marks) was already stripped as individual chars, which is
/// how the asymmetry hid: the set looked deliberate.
const INVISIBLE_RANGES: [std::ops::RangeInclusive<char>; 11] = [
    '\u{0600}'..='\u{0605}',   // Arabic number/year/footnote/safha signs
    '\u{0890}'..='\u{0891}',   // Arabic pound/piastre marks
    '\u{202a}'..='\u{202e}',   // LRE RLE PDF LRO RLO
    '\u{2060}'..='\u{206f}',   // WJ, invisible operators, isolates, deprecated
    '\u{fff0}'..='\u{fffb}',   // reserved default-ignorables + annotation trio
    '\u{13430}'..='\u{1343f}', // Egyptian hieroglyph format controls
    '\u{1bca0}'..='\u{1bca3}', // shorthand format controls
    '\u{1d173}'..='\u{1d17a}', // musical format controls
    // The rest of plane 14 that is default-ignorable, split around the
    // variation selectors at E0100–E01EF, which MUST survive.
    '\u{e0080}'..='\u{e00ff}',
    '\u{e01f0}'..='\u{e0fff}',
    // The deprecated tag block: invisible, `Cf` (so `char::is_control` misses
    // it), and NOT dropped by ratatui — a tag attaches to the preceding
    // grapheme cluster and rides into the cell. That smuggles arbitrary hidden
    // ASCII through a line that looks clean, and on into the transcript
    // snapshot and the clipboard.
    '\u{e0000}'..='\u{e007f}',
];

/// Whether this code point is one the display sanitizers delete outright.
pub fn is_bidi_or_zero_width(ch: char) -> bool {
    BIDI_AND_ZERO_WIDTH.contains(&ch) || INVISIBLE_RANGES.iter().any(|range| range.contains(&ch))
}

/// The one rule every sanitizer shares, so the surfaces can never again drift
/// apart: a control character becomes a single space (preserving the column the
/// wrap step already counted for it), and anything [`is_bidi_or_zero_width`] is
/// DELETED. Deletion, not substitution: most of that family measured 0 columns
/// at wrap time so removing them keeps the count exact, the few that measured
/// 1-2 only shrink the line (never past the committed width), and a substitute
/// space would hand every one of them the visible column they were
/// counterfeiting.
pub fn neutralize_char_into(out: &mut String, ch: char) {
    if is_bidi_or_zero_width(ch) {
        return;
    }
    out.push(if ch.is_control() { ' ' } else { ch });
}

/// [`neutralize_char_into`] over a whole string: control characters become
/// spaces, bidi/zero-width are deleted.
///
/// This is the single-line form, for anything that becomes one row of a
/// terminal — a rendered span, a `println!`, an `eprintln!`. Callers that must
/// keep document structure (`\n`, `\t`) need their own rule; see the TUI's
/// clipboard and composer variants.
pub fn neutralize_display_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        neutralize_char_into(&mut out, ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The set, pinned against an INDEPENDENT hardcoded expectation, swept over
    /// every code point in Unicode.
    ///
    /// Independent is the whole point: an earlier version of this test built
    /// its payload out of the production table, so deleting an entry moved both
    /// sides together and the test stayed green — the assertion was a
    /// tautology. The ranges below are written out again here on purpose, so
    /// that dropping one from production, or over-broadening one, both fail.
    #[test]
    fn neutralize_strips_every_invisible_code_point() {
        const EXPECTED: [std::ops::RangeInclusive<u32>; 23] = [
            0x13430..=0x1343f, // Egyptian hieroglyph format controls
            0x1bca0..=0x1bca3, // shorthand format controls
            0x1d173..=0x1d17a, // musical format controls
            0x00ad..=0x00ad,
            0x0600..=0x0605,
            0x061c..=0x061c,
            0x06dd..=0x06dd,
            0x070f..=0x070f,
            0x0890..=0x0891,
            0x08e2..=0x08e2,
            0x115f..=0x1160,
            0x180e..=0x180e,
            0x200b..=0x200b,
            0x200e..=0x200f,
            0x202a..=0x202e,
            0x2028..=0x2029,
            0x2060..=0x206f,
            0x3164..=0x3164,
            0xfeff..=0xfeff,
            0xffa0..=0xffa0,
            0xfff0..=0xfffb,
            0x110bd..=0x110cd,
            0xe0000..=0xe0fff,
        ];
        // 110bd..=110cd and e0000..=e0fff are coarser than the production
        // tables, so those two are checked by membership below rather than by
        // the sweep's equality.
        const COARSE: [std::ops::RangeInclusive<u32>; 2] = [0x110bd..=0x110cd, 0xe0000..=0xe0fff];

        for code in 0..=0x10FFFFu32 {
            let Some(ch) = char::from_u32(code) else {
                continue;
            };
            let expected = EXPECTED.iter().any(|range| range.contains(&code));
            let coarse = COARSE.iter().any(|range| range.contains(&code));
            let actual = is_bidi_or_zero_width(ch);
            if coarse {
                continue;
            }
            assert_eq!(
                actual, expected,
                "U+{code:04X}: production says {actual}, this test expects {expected}"
            );
        }

        // The two coarse windows, spot-checked at their real members and at a
        // member that must survive.
        for code in [
            0x110bd, 0x110cd, 0xe0001, 0xe0020, 0xe007f, 0xe0080, 0xe0fff,
        ] {
            let ch = char::from_u32(code).unwrap();
            assert!(is_bidi_or_zero_width(ch), "U+{code:04X} must be stripped");
        }
        for code in [0x110be, 0x110cc, 0xe0100, 0xe01ef] {
            let ch = char::from_u32(code).unwrap();
            assert!(!is_bidi_or_zero_width(ch), "U+{code:04X} must survive");
        }
    }

    /// The joiners and variation selectors must NOT be stripped: they modify
    /// real graphemes, and removing them corrupts emoji and Persian text. They
    /// stay safe by riding in the preceding cluster's cell instead.
    #[test]
    fn joiners_and_variation_selectors_survive() {
        for ch in ['\u{200c}', '\u{200d}', '\u{fe0f}', '\u{fe00}', '\u{e0100}'] {
            assert!(
                !is_bidi_or_zero_width(ch),
                "U+{:04X} must survive",
                ch as u32
            );
        }
    }

    #[test]
    fn controls_become_spaces_and_invisibles_vanish() {
        assert_eq!(neutralize_display_text("a\u{1b}[2Kb"), "a [2Kb");
        assert_eq!(neutralize_display_text("a\rb"), "a b");
        assert_eq!(neutralize_display_text("a\u{202e}b"), "ab");
        assert_eq!(neutralize_display_text("a\u{2028}b"), "ab");
        // A joiner rides through untouched.
        assert_eq!(neutralize_display_text("a\u{200d}b"), "a\u{200d}b");
    }
}
