use super::*;

/// Streams `doc` into the incremental renderer in small byte chunks and
/// requires every step to match a full parse of the same text. This is
/// the safety net for `safe_split`: any state the split fails to carry
/// (fence, math, table, blank collapse) shows up as a mismatch mid-stream.
fn assert_incremental_matches_full(doc: &str) {
    let hl = Highlighter::new();
    let mut live = LiveRender::new();
    let mut text = String::new();
    let bytes: Vec<&[u8]> = doc.as_bytes().chunks(7).collect();
    for (step, chunk) in bytes.iter().enumerate() {
        // ASCII docs only in this test: a chunk never splits a code point
        text.push_str(std::str::from_utf8(chunk).unwrap());
        let incremental = live.lines(7, &text, 60, &hl);
        let full = render(&text, 60, &hl);
        assert_eq!(
            incremental,
            full,
            "mismatch after step {step} at {} bytes",
            text.len()
        );
    }
    // replacing the whole answer (finish_turn backfill) must fall back
    // cleanly: guards fail, full parse, prefix re-established
    let replaced = format!("{doc}\n\nAuthoritative tail.");
    let incremental = live.lines(7, &replaced, 60, &hl);
    assert_eq!(incremental, render(&replaced, 60, &hl));
}

#[test]
fn incremental_matches_full_prose_and_code() {
    assert_incremental_matches_full(
        "Intro paragraph with **bold** and `inline`.\n\n## Heading two\n\n\
         A longer paragraph that keeps growing well past one visual row so the\n\
         wrap has real work to do.\n\n```rust\nfn main() {\n    let x = 1;\n    let y = 2;\n}\n```\n\n\
         Fence closed, prose again, and a code span `x` with a very long\ncontinuation line.",
    );
}

#[test]
fn incremental_matches_full_constructs() {
    assert_incremental_matches_full(
        "| a | b |\n|---|---|\n| 1 | 2 |\n\n$$\nx = y + 1\n$$\n\n> quoted line\n> more quote\n\n\
         - one\n- two\n  - nested\n\n1. first\n2. second\n\n---\n\n> A quote with a table?\n\n| p | q |\n| :-- | --: |\n| r | s |\n\n\
         setext shaped: alpha\n---\n\nbeta\n===\n\ngamma\n------",
    );
}

#[test]
fn incremental_survives_fence_with_blank_lines() {
    assert_incremental_matches_full(
        "Lead text.\n\n```text\nline one\n\nline two after a blank\n\nmore\n```\n\nTrailing prose.",
    );
}

#[test]
fn incremental_survives_math_with_blanks() {
    assert_incremental_matches_full("$$\na = b\n\nc = d\n$$\n\nAfter math.");
}

#[test]
fn incremental_survives_setext_shaped_lines() {
    // "foo\n---" / "bar\n===" — a cut between the text and its underline
    // would freeze a paragraph-shaped prefix in the store while the tail
    // re-renders the same bytes differently, so the split must never land
    // there. Streamed byte-by-byte to hit every intermediate state.
    assert_incremental_matches_full("Lead.\n\nfoo\n---\n\nbar\n===\n\nbaz\n-\n\ntail");
}

#[test]
fn safe_split_never_lands_inside_fence_or_math() {
    let doc =
        "a\n\n```rust\nx\n\ny\n```\n\n| t | u |\n|---|---|\n| v | w |\n\n$$\nk\n\nm\n$$\n\nend";
    let hl = Highlighter::new();
    let mut live = LiveRender::new();
    // stream byte-by-byte: every intermediate cut must keep the split
    // identity, which the full-render comparison checks per step
    let mut text = String::new();
    for b in doc.bytes() {
        text.push(b as char);
        assert_eq!(
            live.lines(3, &text, 60, &hl),
            render(&text, 60, &hl),
            "at byte {}",
            text.len()
        );
    }
}

#[test]
fn wrap_preserves_explicit_newlines() {
    // Note: `Line::from(&str)` already splits on newlines in this
    // ratatui version, so build the span explicitly to cover the
    // newline-inside-span path (tool output, pasted text).
    let input = Line::from(vec![Span::styled("alpha\nbeta".to_string(), Theme::base())]);
    let (rows, _) = wrap_tagged(vec![(input, Some(1))], 80);

    let text: Vec<_> = rows.iter().map(line_text_pub).collect();
    assert_eq!(text, vec!["alpha", "beta"]);
}

#[test]
fn wrap_collapses_only_at_line_breaks_not_mid_line() {
    // trailing newline must not append a phantom blank row
    let input = Line::from("alpha\n");
    let (rows, _) = wrap_tagged(vec![(input, None)], 80);
    let text: Vec<_> = rows.iter().map(line_text_pub).collect();
    assert_eq!(text, vec!["alpha"]);
}

#[test]
fn wrap_expands_tabs_to_terminal_stops() {
    // `read` numbers rows as `{:>6}\tline`: a raw tab would reach the
    // terminal as a cursor jump the buffer model cannot see, desyncing
    // the row and spraying neighbor fragments. Six columns in, the next
    // multiple-of-8 stop is column 8.
    let input = Line::from(vec![Span::styled(
        "     1\t[package]".to_string(),
        Theme::base(),
    )]);
    let (rows, _) = wrap_tagged(vec![(input, Some(1))], 80);
    let text: Vec<_> = rows.iter().map(line_text_pub).collect();
    assert_eq!(text, vec!["     1  [package]"]);
}

#[test]
fn wrap_keeps_only_text_after_carriage_return() {
    // progress-bar overwrites: only the visible tail survives
    let input = Line::from(vec![Span::styled("50%\r100%".to_string(), Theme::base())]);
    let (rows, _) = wrap_tagged(vec![(input, None)], 80);
    let text: Vec<_> = rows.iter().map(line_text_pub).collect();
    assert_eq!(text, vec!["100%"]);
}

#[test]
fn wrap_drops_other_control_characters() {
    let input = Line::from(vec![Span::styled("a\x07b\x00c".to_string(), Theme::base())]);
    let (rows, _) = wrap_tagged(vec![(input, None)], 80);
    let text: Vec<_> = rows.iter().map(line_text_pub).collect();
    assert_eq!(text, vec!["abc"]);
}

#[test]
fn hostile_model_output_cannot_emit_escapes() {
    // untrusted content (tool output, pasted text) reaches the
    // terminal through these rows: a CSI/OSC sequence must die here,
    // never in the buffer. Use the full render entry, not internals.
    let hostile = "done\x1b[2J\x1b[1;1Hwiped\x1b]8;;http://evil\x07link\nsecond\x1b[31mred";
    let hl = Highlighter::new();
    for line in render(hostile, 80, &hl) {
        for span in &line.spans {
            assert!(
                !span.content.chars().any(|c| c.is_control()),
                "escape survived: {span:?}"
            );
        }
    }
    let text: Vec<_> = render(hostile, 80, &hl).iter().map(line_text_pub).collect();
    let joined = text.join("\n");
    assert!(joined.contains("done"), "{joined:?}");
    assert!(joined.contains("second"), "{joined:?}");
    assert!(!joined.contains('\x1b'), "{joined:?}");
}

#[test]
fn inline_many_markers_stays_linear() {
    let text = "*x* ".repeat(20_000);
    let spans = inline(&text, Theme::base());
    // each pair yields one italic span plus its trailing literal space
    assert_eq!(spans.len(), 40_000);
    let joined: String = spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(joined, "x ".repeat(20_000));
}

#[test]
fn inline_unclosed_brackets_stay_linear() {
    let text = "[".repeat(20_000);
    let spans = inline(&text, Theme::base());
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].content.len(), 20_000);
}

#[test]
fn inline_underscore_runs_stay_linear() {
    // opener at 0 scans a bounded window, then goes literal: each
    // repetition costs O(1) amortized instead of re-scanning the tail
    let text = "_a ".repeat(5_000);
    let spans = inline(&text, Theme::base());
    let joined: String = spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(joined, text);
}

#[test]
fn table_narrow_cjk_stays_rectangular() {
    let hl = Highlighter::new();
    // forces minimum-width columns filled with double-width glyphs
    let md = "| a | b | c |\n|---|---|---|\n| 日 | 本 | 語 |\n| 本 | 語 | 日 |\n";
    let lines = render(md, 20, &hl);
    let cols: Vec<usize> = lines
        .iter()
        .map(|l| UnicodeWidthStr::width(line_text_pub(l).as_str()))
        .collect();
    assert!(!cols.is_empty());
    assert!(
        cols.iter().all(|&c| c == cols[0]),
        "ragged narrow grid {cols:?}"
    );
}

#[test]
fn highlight_cache_hits_repeats_and_evicts_oldest() {
    let hl = Highlighter::new();
    let code = "fn main() {}\n";
    let first = hl.highlight_wrapped(code, Some("rust"), 80);
    // case-insensitive lang shares the key with the first render
    let second = hl.highlight_wrapped(code, Some("RUST"), 80);
    assert_eq!(first.len(), second.len());
    assert_eq!(hl.cache_len(), 1);
    // width is part of the key
    hl.highlight_wrapped(code, Some("rust"), 40);
    assert_eq!(hl.cache_len(), 2);
    for i in 0..40 {
        hl.highlight_wrapped(&format!("let x{i} = {i};\n"), Some("rust"), 80);
    }
    assert_eq!(hl.cache_len(), 32);
}

#[test]
fn highlight_caps_lines_and_width_without_dropping_text() {
    let hl = Highlighter::new();
    // past 400 lines the block stays fully visible, just uncolored
    let code = "let x = 1;\n".repeat(500);
    let t0 = std::time::Instant::now();
    let lines = hl.highlight_code(&code, Some("rust"));
    assert_eq!(lines.len(), 500, "1:1 with input lines");
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(5),
        "bounded: {:?}",
        t0.elapsed()
    );
    let colored = lines[..400]
        .iter()
        .any(|l| l.spans.iter().any(|s| s.style.fg != Some(Color::Gray)));
    assert!(colored, "head stays highlighted");
    assert!(
        lines[400..]
            .iter()
            .all(|l| l.spans.iter().all(|s| s.style.fg == Some(Color::Gray))),
        "tail falls back to plain"
    );
    // a single huge line renders plain, never dropped
    let wide = "x".repeat(3000) + "\n";
    let lines = hl.highlight_code(&wide, Some("rust"));
    assert_eq!(lines.len(), 1);
    let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(text.len(), 3000, "full text survives");
    assert!(
        lines[0]
            .spans
            .iter()
            .all(|s| s.style.fg == Some(Color::Gray)),
        "huge line is plain"
    );
}

#[test]
fn code_block_renders_plain_lines_without_frame() {
    let hl = Highlighter::new();
    let lines = render("```rust\nfn main() {}\n```\ntext", 80, &hl);
    assert!(lines.len() >= 2);
    let first = line_text_pub(&lines[0]);
    assert_eq!(first, "fn main() {}");
    let joined: String = lines.iter().map(|l| line_text_pub(l)).collect();
    assert!(joined.contains("text"), "{joined:?}");
    // No frame borders and no language badge (Codex-style plain block).
    for ch in ['╭', '╮', '╰', '╯', '│'] {
        assert!(!joined.contains(ch), "frame leaked: {joined:?}");
    }
    let narrow = render(
        "```cpp\nstd::cout << \"Hello, world!\" << std::endl;\n```",
        24,
        &hl,
    );
    for line in &narrow {
        assert!(UnicodeWidthStr::width(line_text_pub(line).as_str()) <= 24);
    }
}

#[test]
fn headings_hide_markers_keep_level_style() {
    let hl = Highlighter::new();
    let lines = render("# Catalog\ndescription\n# Additional example", 80, &hl);
    assert_eq!(lines.len(), 3);
    let first: String = lines[0]
        .spans
        .iter()
        .map(|s| s.content.to_string())
        .collect();
    // markers are syntax, not content: never printed
    assert_eq!(first, "Catalog");
    assert!(
        lines[0]
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::BOLD))
    );
    // h1 wears the brand gradient, no underline; deeper levels are not.
    let h1fg: Vec<Option<ratatui::style::Color>> =
        lines[0].spans.iter().map(|s| s.style.fg).collect();
    assert!(
        h1fg.iter()
            .all(|fg| !matches!(fg, Some(ratatui::style::Color::Reset) | None)),
        "h1 must be painted: {:?}",
        lines[0]
    );
    assert!(
        lines[0]
            .spans
            .iter()
            .all(|s| !s.style.add_modifier.contains(Modifier::UNDERLINED)),
        "h1 has no underline: {:?}",
        lines[0]
    );
    let h2 = render("## Sub", 80, &hl);
    let h2text: String = h2[0].spans.iter().map(|s| s.content.to_string()).collect();
    assert_eq!(h2text, "Sub");
    assert!(
        h2[0]
            .spans
            .iter()
            .all(|s| s.style.add_modifier.contains(Modifier::BOLD)
                && !s.style.add_modifier.contains(Modifier::UNDERLINED)),
        "h2 must be bold, not underlined: {:?}",
        h2[0]
    );
    let h3 = render("### Deep", 80, &hl);
    let h3text: String = h3[0].spans.iter().map(|s| s.content.to_string()).collect();
    assert_eq!(h3text, "Deep");
    assert!(
        h3[0]
            .spans
            .iter()
            .all(|s| s.style.add_modifier.contains(Modifier::BOLD)
                && s.style.add_modifier.contains(Modifier::ITALIC)
                && !matches!(s.style.fg, Some(ratatui::style::Color::Gray) | None)),
        "h3 must be bright bold+italic, never body gray: {:?}",
        h3[0]
    );
    // closed ATX (`### Deep ###`) drops the closing run too
    let closed = render("### Deep ###", 80, &hl);
    let closed_text: String = closed[0]
        .spans
        .iter()
        .map(|s| s.content.to_string())
        .collect();
    assert_eq!(closed_text, "Deep");
    let h4 = render("#### Fine", 80, &hl);
    assert!(
        h4[0]
            .spans
            .iter()
            .all(|s| s.style.add_modifier.contains(Modifier::BOLD)
                && s.style.add_modifier.contains(Modifier::ITALIC)
                && s.style.fg == Some(ratatui::style::Color::White)),
        "h4 must be white bold+italic: {:?}",
        h4[0]
    );
    // `#Foo` is a paragraph, not a heading: the marker stays visible
    let nospace = render("#NoSpace", 80, &hl);
    let text: String = nospace
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(text.contains("#NoSpace"), "{text:?}");
}

#[test]
fn inline_styles() {
    let spans = inline("**bold** and `code`", Theme::base());
    let all: String = spans.iter().map(|s| s.content.to_string()).collect();
    assert_eq!(all, "bold and code");
    assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn wrap_carries_tags() {
    let line = Line::from(Span::styled("a ".repeat(50), Theme::base()));
    let (rows, tags) = wrap_tagged(vec![(line, Some(7))], 20);
    assert!(rows.len() > 1);
    assert!(tags.iter().all(|t| *t == Some(7)));
}

#[test]
fn table_renders_grid() {
    let hl = Highlighter::new();
    let md = "| a | b |\n|---|---|\n| 1 | 2 |\n";
    let lines = render(md, 40, &hl);
    let joined: Vec<String> = lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
        .collect();
    assert!(joined.iter().any(|r| r.contains('┌')), "{joined:?}");
    assert!(joined.iter().any(|r| r.contains(" a ")), "{joined:?}");
}

#[test]
fn bold_inline_parses() {
    let spans = inline("**Memory management**", Theme::base());
    let text: String = spans.iter().map(|s| s.content.to_string()).collect();
    assert_eq!(text, "Memory management");
    assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn table_cells_render_inline_markdown() {
    let hl = Highlighter::new();
    let md = "| a | b |\n|---|---|\n| **x** | y |\n";
    let lines = render(md, 40, &hl);
    let all: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect::<Vec<_>>()
        .join("");
    assert!(!all.contains("**"), "{all:?}");
    assert!(all.contains('x'));
}

#[test]
fn table_wraps_long_cells_instead_of_truncating() {
    let hl = Highlighter::new();
    let md = "| col | desc |\n|-----|------|\n| k | alpha beta gamma delta epsilon |\n";
    let lines = render(md, 30, &hl);
    let all: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(
        !all.contains('…'),
        "cell text must wrap, not truncate: {all:?}"
    );
    assert!(all.contains("epsilon"), "tail lost: {all:?}");
    // every rendered row stays within the requested width
    for l in &lines {
        let wdt: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
        assert!(wdt <= 30, "row too wide ({wdt}): {l:?}");
    }
}

/// Every line of a table — borders and rows alike — must occupy the same
/// number of terminal columns. Measuring cells in characters instead of
/// columns made the grid ragged for any non-Latin text: a CJK glyph takes
/// two cells, so `"─".repeat(w)` came out shorter than the row it framed
/// and the right border landed inside the text.
#[test]
fn table_grid_is_rectangular_with_wide_and_cyrillic_text() {
    let hl = Highlighter::new();
    let md = "| lang | note |\n|---|---|\n| 日本語 | データ |\n| кириллица | текст |\n";
    for width in [40u16, 60, 80] {
        let lines = render(md, width, &hl);
        let cols: Vec<usize> = lines
            .iter()
            .map(|l| UnicodeWidthStr::width(line_text_pub(l).as_str()))
            .collect();
        assert!(!cols.is_empty(), "width {width}: nothing rendered");
        assert!(
            cols.iter().all(|&c| c == cols[0]),
            "width {width}: ragged grid {cols:?}"
        );
        assert!(
            cols[0] <= width as usize,
            "width {width}: grid overflows the terminal {cols:?}"
        );
    }
}

#[test]
fn table_narrow_columns_keep_whole_tokens() {
    // regression: proportional shrink squeezed short columns to MIN_COL
    // and shredded `file_remove` into one letter per row, while the
    // longest token always overflowed its own `wi - 2` budget by the two
    // unpaid padding columns
    let hl = Highlighter::new();
    let md = "| Инструмент | Категория | Мутирует | Чекпоинт / Undo | Соответствие the design |\n|---|---|---|---|---|\n| `file_remove` | files | Да | Да (blob_before) | Устраняет обходные пути через `bash rm`, сохраняет инвариант хостовых диффов |\n| `git_st` | git | Да | Да | Закрывает разрыв: позволяет коммитить untracked файлы |\n";
    let lines = render(md, 100, &hl);
    let text: Vec<String> = lines.iter().map(|l| line_text_pub(l)).collect();
    for token in ["Инструмент", "file_remove", "Категория", "git_st"] {
        assert!(
            text.iter().any(|l| l.contains(token)),
            "{token} shredded across rows: {text:?}"
        );
    }
    let cols: Vec<usize> = lines
        .iter()
        .map(|l| UnicodeWidthStr::width(line_text_pub(l).as_str()))
        .collect();
    assert!(cols.iter().all(|&c| c == cols[0]), "ragged grid {cols:?}");
    assert!(cols[0] <= 100, "grid overflows: {cols:?}");
}

#[test]
fn table_wraps_long_unbreakable_words_and_urls() {
    let hl = Highlighter::new();
    let md = "| col | url |\n|---|---|\n| a | https://example.com/a/very/long/unbreakable/link/that/exceeds/the/column/budget/by/far |\n| b | prefix https://another.example.com/very/long/path/with/words/and/no/space/runs |\n";
    for width in [30u16, 40, 60] {
        let lines = render(md, width, &hl);
        let cols: Vec<usize> = lines
            .iter()
            .map(|l| UnicodeWidthStr::width(line_text_pub(l).as_str()))
            .collect();
        assert!(!cols.is_empty(), "width {width}: nothing rendered");
        assert!(
            cols.iter().all(|&c| c == cols[0]),
            "width {width}: ragged grid {cols:?}"
        );
        assert!(
            cols[0] <= width as usize,
            "width {width}: grid overflows terminal ({w} > {width}): {cols:?}",
            w = cols[0]
        );
    }
}

#[test]
fn code_block_rows_fit_width() {
    let hl = Highlighter::new();
    let lines = render("```rust\nfn a() {}\nlet x = 12345;\n```\n", 60, &hl);
    assert!(!lines.is_empty());
    for l in &lines {
        assert!(
            UnicodeWidthStr::width(line_text_pub(l).as_str()) <= 60,
            "{l:?}"
        );
    }
    let joined: String = lines.iter().map(|l| line_text_pub(l)).collect();
    assert!(joined.contains("fn a()"), "{joined:?}");
}

#[test]
fn code_block_language_badge_dropped() {
    let hl = Highlighter::new();
    let lines = render("```javascript\n1\n```\n", 60, &hl);
    let joined: String = lines.iter().map(|l| line_text_pub(l)).collect();
    assert!(joined.contains('1'), "{joined:?}");
    // No frame and no language badge: plain code row only.
    assert_eq!(joined, "1", "{joined:?}");
}

#[test]
fn intra_word_underscores_stay_literal() {
    let spans = inline("some_var_name and a*b*c", Theme::base());
    let text: String = spans.iter().map(|s| s.content.to_string()).collect();
    // emphasis markers are consumed, so `*b*` collapses to `b`
    assert_eq!(text, "some_var_name and abc");
    // no italic leaked from the identifier
    assert!(
        spans
            .iter()
            .filter(|s| s.content.contains("some_var_name"))
            .all(|s| !s.style.add_modifier.contains(Modifier::ITALIC))
    );
    // `*` keeps working inside words
    assert!(
        spans
            .iter()
            .any(|s| s.content == "b" && s.style.add_modifier.contains(Modifier::ITALIC)),
        "{spans:?}"
    );
}

#[test]
fn escapes_and_triple_markers() {
    let spans = inline(r"\*literal\* and ***both*** and ~~gone~~", Theme::base());
    let text: String = spans.iter().map(|s| s.content.to_string()).collect();
    assert_eq!(text, "*literal* and both and gone", "{text:?}");
    let both = spans.iter().find(|s| s.content == "both").unwrap();
    assert!(both.style.add_modifier.contains(Modifier::BOLD));
    assert!(
        !both.style.add_modifier.contains(Modifier::ITALIC),
        "*** folds to bold: {both:?}"
    );
    let gone = spans.iter().find(|s| s.content == "gone").unwrap();
    assert!(gone.style.add_modifier.contains(Modifier::CROSSED_OUT));
    // double backticks span greedily across single ones
    let code = inline("``a ` b``", Theme::base());
    let code_text: String = code.iter().map(|s| s.content.to_string()).collect();
    assert_eq!(code_text, "a ` b", "{code_text:?}");
}

/// char index where `needle` starts in rendered span text.
fn col_of(spans: &[Span<'_>], needle: &str) -> usize {
    let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
    text.find(needle)
        .map(|b| text[..b].chars().count())
        .unwrap_or(usize::MAX)
}

#[test]
fn link_hit_resolves_label_parens_and_autolinks() {
    // md link: click on the label opens the adjacent parens URL
    let row = inline("see [docs](https://example.com/x) now", Theme::base());
    assert_eq!(
        link_url_at(&row, None, col_of(&row, "docs")),
        Some("https://example.com/x".to_string())
    );
    // click on plain text: nothing
    assert_eq!(link_url_at(&row, None, 0), None);
    // click on the dim parens (not link-styled): nothing
    assert_eq!(link_url_at(&row, None, col_of(&row, "(https")), None);
    // autolink: click on the URL itself
    let auto = inline("go <https://example.com> ok", Theme::base());
    assert_eq!(
        link_url_at(&auto, None, col_of(&auto, "https")),
        Some("https://example.com".to_string())
    );
    // bare URL in code text is literal, never a link
    let code = inline("run `https://example.com` ok", Theme::base());
    assert_eq!(link_url_at(&code, None, col_of(&code, "https")), None);
}

#[test]
fn link_hit_never_guesses_across_links_or_rows() {
    // two links one row: each label binds its own parens
    let row = inline(
        "[a](https://a.example/1) [b](https://b.example/2)",
        Theme::base(),
    );
    let text: String = row.iter().map(|s| s.content.as_ref()).collect();
    let a_col = text.find('a').unwrap();
    let b_col = text.find(" b (").unwrap() + 1;
    assert_eq!(
        link_url_at(&row, None, a_col),
        Some("https://a.example/1".to_string())
    );
    assert_eq!(
        link_url_at(&row, None, b_col),
        Some("https://b.example/2".to_string())
    );
    // label at the row edge, parens opening the next row (wrap ate the space)
    let full = inline("[long label](https://example.com/deep)", Theme::base());
    let full_text: String = full.iter().map(|s| s.content.as_ref()).collect();
    let cut = full_text.find("(https").unwrap();
    let (h1, h2) = full_text.split_at(cut);
    let top = vec![Span::styled(
        h1.to_string(),
        Style::new()
            .fg(Theme::ACCENT())
            .add_modifier(Modifier::UNDERLINED),
    )];
    let bottom = vec![Span::styled(h2.to_string(), Theme::base())];
    assert_eq!(
        link_url_at(&top, Some(&bottom), col_of(&top, "label")),
        Some("https://example.com/deep".to_string())
    );
    // autolink cut at the row edge with text ahead: abstain, never glue
    let edge = vec![Span::styled(
        "https://example.com".to_string(),
        Style::new()
            .fg(Theme::ACCENT())
            .add_modifier(Modifier::UNDERLINED),
    )];
    let ahead = vec![Span::styled("more text".to_string(), Theme::base())];
    assert_eq!(link_url_at(&edge, Some(&ahead), 8), None);
    // exotic schemes never resolve, even when styled as links
    let evil = inline("[x](javascript:alert(1))", Theme::base());
    assert_eq!(link_url_at(&evil, None, 0), None);
}

#[test]
fn links_render_text_and_dim_url() {
    let spans = inline("see [docs](https://example.com/x) now", Theme::base());
    let text: String = spans.iter().map(|s| s.content.to_string()).collect();
    assert_eq!(text, "see docs (https://example.com/x) now", "{text:?}");
    assert!(
        spans
            .iter()
            .any(|s| s.content == "docs" && s.style.add_modifier.contains(Modifier::UNDERLINED)),
        "{spans:?}"
    );
    let auto = inline("go <https://example.com> ok", Theme::base());
    let auto_text: String = auto.iter().map(|s| s.content.to_string()).collect();
    assert_eq!(auto_text, "go https://example.com ok", "{auto_text:?}");
    // emphasis markers inside the URL must not leak out
    let tricky = inline("[a](https://x/y*z) tail", Theme::base());
    let tricky_text: String = tricky.iter().map(|s| s.content.to_string()).collect();
    assert_eq!(tricky_text, "a (https://x/y*z) tail", "{tricky_text:?}");
}

#[test]
fn lists_tasks_nesting_and_ordered_guards() {
    let hl = Highlighter::new();
    let lines = render(
        "- [ ] todo\n- [x] done\n  - nested\n1) kept\n1.foo\n-nope",
        80,
        &hl,
    );
    let all: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(all.contains("[ ]"), "{all:?}");
    assert!(all.contains("[x]"), "{all:?}");
    assert!(
        all.contains("1)"),
        "`)` delimiter must be preserved: {all:?}"
    );
    assert!(all.contains("1.foo"), "`1.foo` is a paragraph: {all:?}");
    assert!(all.contains("-nope"), "`-nope` is a paragraph: {all:?}");
    // nested item is indented deeper than the top-level one
    let top = lines
        .iter()
        .find(|l| {
            l.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
                .contains("todo")
        })
        .unwrap();
    let nested = lines
        .iter()
        .find(|l| {
            l.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
                .contains("nested")
        })
        .unwrap();
    let indent = |l: &Line<'_>| line_text_pub(l).chars().take_while(|c| *c == ' ').count();
    assert!(indent(nested) > indent(top), "{top:?} vs {nested:?}");
}

#[test]
fn quotes_nest_and_hr_allows_spaces() {
    let hl = Highlighter::new();
    let lines = render(">> deep\n> shallow\n* * *", 80, &hl);
    let all: String = lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(all.contains("▐ ▐ deep"), "{all:?}");
    assert!(all.contains("▐ shallow"), "{all:?}");
    assert!(all.contains('─'), "spaced `* * *` is a rule: {all:?}");
}

#[test]
fn quote_rail_repeats_on_every_wrapped_row() {
    let hl = Highlighter::new();
    let lines = render(
        "> alpha beta gamma delta epsilon zeta eta theta iota kappa",
        20,
        &hl,
    );
    assert!(lines.len() > 1, "must wrap at width 20: {lines:?}");
    for l in &lines {
        let text = line_text_pub(l);
        assert!(
            text.starts_with("▐ "),
            "continuation row lost its rail: {text:?}"
        );
        assert!(
            UnicodeWidthStr::width(text.as_str()) <= 20,
            "row overflows: {text:?}"
        );
    }
    let all: String = lines.iter().map(|l| line_text_pub(l)).collect();
    assert!(all.contains("kappa"), "tail lost: {all:?}");
}

#[test]
fn quote_text_stays_green_without_band() {
    let hl = Highlighter::new();
    let lines = render("> hi", 10, &hl);
    assert_eq!(lines.len(), 1);
    let text = line_text_pub(&lines[0]);
    assert_eq!(text, "▐ hi      ", "{text:?}");
    // rail glyph itself is transparent; the body pads plain too —
    // no band, the green text and rail carry the quote alone
    assert_eq!(lines[0].spans[0].style.bg, None);
    assert!(
        lines[0].spans[1..].iter().all(|s| s.style.bg.is_none()),
        "quote body has no band bg: {:?}",
        lines[0]
    );
}

#[test]
fn quote_inline_markup_stays_green_without_band() {
    let hl = Highlighter::new();
    let lines = render("> use `read` and **bold** here", 60, &hl);
    assert_eq!(lines.len(), 1);
    let spans = &lines[0].spans;
    // inline code inside a quote takes the green fg, no band bg
    let code = spans
        .iter()
        .find(|s| s.content == "read")
        .expect("code span");
    assert_eq!(code.style.fg, Some(Theme::GREEN()));
    assert_eq!(code.style.bg, None);
    // bold keeps the green fg, no band bg
    let bold = spans
        .iter()
        .find(|s| s.content == "bold")
        .expect("bold span");
    assert_eq!(bold.style.fg, Some(Theme::GREEN()));
    assert_eq!(bold.style.bg, None);
    // top-level code stays transparent (no ambient bg to inherit)
    let plain = inline("`x`", Theme::base());
    assert_eq!(plain[0].style.bg, None);
}

#[test]
fn antigravity_recipe_gray_prose_white_emphasis_accent_links() {
    use ratatui::style::Color;
    let hl = Highlighter::new();
    // prose is dim gray
    let lines = render("plain words here", 60, &hl);
    assert!(
        lines[0]
            .spans
            .iter()
            .all(|s| s.style.fg == Some(Color::Gray))
    );
    // bold and code go bright white on prose
    let lines = render("a **strong** move with `code`", 60, &hl);
    let spans = &lines[0].spans;
    assert_eq!(
        spans
            .iter()
            .find(|s| s.content == "strong")
            .expect("bold")
            .style
            .fg,
        Some(Color::White)
    );
    assert_eq!(
        spans
            .iter()
            .find(|s| s.content == "code")
            .expect("code")
            .style
            .fg,
        Some(Color::White)
    );
    // links are the sparse accent, underlined
    let lines = render("see [docs](https://example.com) now", 60, &hl);
    let link = lines[0]
        .spans
        .iter()
        .find(|s| s.content == "docs")
        .expect("link");
    assert_eq!(link.style.fg, Some(Theme::ACCENT()));
    assert!(link.style.add_modifier.contains(Modifier::UNDERLINED));
}

#[test]
fn lone_dollar_lines_delimit_math_like_double() {
    let hl = Highlighter::new();
    // models habitually write lone `$` delimiters: same collapse
    let lines = render("$\nx + y\n$", 80, &hl);
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!text.contains('$'), "delimiters collapse: {text:?}");
    assert!(text.contains("x + y"), "{text:?}");
}

#[test]
fn list_continuations_hang_under_the_text() {
    let hl = Highlighter::new();
    // narrow frame forces a wrap: continuation rows align under the
    // body, never under the marker or at column zero
    let lines = render("- alpha beta gamma delta epsilon", 20, &hl);
    assert!(lines.len() >= 2, "must wrap: {lines:?}");
    let text: Vec<String> = lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
        .collect();
    assert!(text[0].starts_with("  - alpha"), "{text:?}");
    for row in &text[1..] {
        assert!(
            row.starts_with("    ") && row[4..].starts_with(|c: char| !c.is_whitespace()),
            "hanging indent under the body: {text:?}"
        );
    }
    // ordered markers: wider head, same rule
    let lines = render("1. alpha beta gamma delta epsilon", 20, &hl);
    let text: Vec<String> = lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
        .collect();
    assert!(lines.len() >= 2, "must wrap: {text:?}");
    assert!(text[0].starts_with("  1. alpha"), "{text:?}");
    for row in &text[1..] {
        assert!(
            row.starts_with("     ") && row[5..].starts_with(|c: char| !c.is_whitespace()),
            "hanging indent under the body: {text:?}"
        );
    }
}

#[test]
fn tables_align_pipes_and_escapes() {
    let hl = Highlighter::new();
    // outer pipes optional, alignment honored, `\|` stays in the cell
    // (wrapping to a second physical row when the column is narrow)
    let md = "a | b\n--: | :-:\n1 | 2\nx \\| y | z\n";
    let lines = render(md, 40, &hl);
    let joined: Vec<String> = lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
        .collect();
    assert!(joined.iter().any(|r| r.contains('┌')), "{joined:?}");
    let all = joined.join("\n");
    // the escaped pipe is content, not a delimiter: every body row keeps
    // exactly 2 columns (3 rails), and the cell text survives the wrap
    for l in &lines {
        if line_text_pub(l).contains('│') {
            assert_eq!(
                line_text_pub(l).chars().filter(|c| *c == '│').count(),
                3,
                "{l:?}"
            );
        }
    }
    assert!(
        all.contains('x') && all.contains("| y"),
        "escaped pipe lost: {all:?}"
    );
    // right-aligned numeric cell: padding sits left of the digit
    let body = lines
        .iter()
        .find(|l| line_text_pub(l).contains('1') && line_text_pub(l).contains('│'))
        .unwrap();
    let t = line_text_pub(body);
    let cell = t.split('│').nth(1).unwrap();
    assert!(
        cell.starts_with("  ") || cell.starts_with("   "),
        "right column must pad left: {cell:?}"
    );
    for l in &lines {
        assert!(
            UnicodeWidthStr::width(line_text_pub(l).as_str()) <= 40,
            "{l:?}"
        );
    }

    // Pipes inside inline code spans (e.g. `area.width < 20 || height < 6`)
    // must not split the cell into phantom empty columns.
    let code_md = "| Критерий | Балл | Основание |\n|---|---|---|\n| Узкие | 80 | проверка `area.width < 20 || height < 6` в коде |\n";
    let code_lines = render(code_md, 80, &hl);
    for l in &code_lines {
        if line_text_pub(l).contains('│') {
            assert_eq!(
                line_text_pub(l).chars().filter(|c| *c == '│').count(),
                4,
                "3 columns (4 rails) even with `||` inside backticks: {l:?}"
            );
        }
    }
}

#[test]
fn code_block_survives_narrow_width_and_unknown_lang() {
    let hl = Highlighter::new();
    // `foobarlang` is unknown -> plain-text fallback, no frame
    let lines = render("```foobarlang\nlet x = 1;\n```", 40, &hl);
    assert!(!lines.is_empty());
    // degenerate viewport: nothing overflows
    let tiny = render("```rust\nfn main() {}\n```", 4, &hl);
    assert!(!tiny.is_empty());
    for l in &tiny {
        assert!(
            UnicodeWidthStr::width(line_text_pub(l).as_str()) <= 4,
            "{l:?}"
        );
    }
    // uppercase language tag still highlights instead of falling back:
    // the body keeps syntax spans, not one uniform style
    let up = render("```RUST\nfn main() {}\n```", 60, &hl);
    assert_eq!(up.len(), 1);
    let styles: std::collections::HashSet<String> = up[0]
        .spans
        .iter()
        .map(|s| format!("{:?}", s.style))
        .collect();
    assert!(styles.len() > 1, "expected highlight spans: {:?}", up[0]);
}

#[test]
fn math_renders_arrows_and_latex_symbols() {
    let spans = inline(r"$\rightarrow$", Theme::base());
    let text: String = spans.iter().map(|s| s.content.to_string()).collect();
    assert_eq!(text, "→");

    let spans = inline(r"Step 1 $\rightarrow$ Step 2", Theme::base());
    let text: String = spans.iter().map(|s| s.content.to_string()).collect();
    assert_eq!(text, "Step 1 → Step 2");

    let spans = inline(r"$\to$", Theme::base());
    let text: String = spans.iter().map(|s| s.content.to_string()).collect();
    assert_eq!(text, "→");

    let spans = inline(r"$$\rightarrow$$", Theme::base());
    let text: String = spans.iter().map(|s| s.content.to_string()).collect();
    assert_eq!(text, "→");

    let spans = inline(r"A \rightarrow B", Theme::base());
    let text: String = spans.iter().map(|s| s.content.to_string()).collect();
    assert_eq!(text, "A → B");

    let spans = inline(
        r"$\leftarrow$ and $\Leftarrow$ and $\Rightarrow$ and $\leftrightarrow$",
        Theme::base(),
    );
    let text: String = spans.iter().map(|s| s.content.to_string()).collect();
    assert_eq!(text, "← and ⇐ and ⇒ and ↔");

    let spans = inline(r"$\alpha \le \beta \ne \gamma$", Theme::base());
    let text: String = spans.iter().map(|s| s.content.to_string()).collect();
    assert_eq!(text, "α ≤ β ≠ γ");

    let spans = inline(r"**$\rightarrow$**", Theme::base());
    let text: String = spans.iter().map(|s| s.content.to_string()).collect();
    assert_eq!(text, "→");
    assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn currency_and_escaped_dollars_stay_literal() {
    let spans = inline("Price: $10 and fee: $20", Theme::base());
    let text: String = spans.iter().map(|s| s.content.to_string()).collect();
    assert_eq!(text, "Price: $10 and fee: $20");

    let spans = inline(r"Costs \$50 only", Theme::base());
    let text: String = spans.iter().map(|s| s.content.to_string()).collect();
    assert_eq!(text, "Costs $50 only");

    let spans = inline(r"`$\rightarrow$`", Theme::base());
    let text: String = spans.iter().map(|s| s.content.to_string()).collect();
    assert_eq!(text, r"$\rightarrow$");
}

#[test]
fn display_math_block_renders_arrow() {
    let hl = Highlighter::new();
    let md = "$$\n\\rightarrow\n$$\n";
    let lines = render(md, 40, &hl);
    let all: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(all.contains('→'), "rendered: {all:?}");
}

/// Issue #175: a `\r` was treated as an overwrite wherever it appeared — including
/// the CR of a `\r\n`. A tool-output line ending in CRLF therefore cut to the
/// bytes after the last CR, which is nothing, so whole rows disappeared for
/// every Windows-native command.
#[test]
fn crlf_is_a_line_end_not_an_overwrite() {
    let kept: String = sanitize_line(Line::from(Span::raw("first line\r\nsecond line\r\n")))
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(kept.contains("first line"), "line lost: {kept:?}");
    assert!(kept.contains("second line"), "line lost: {kept:?}");
    assert!(!kept.contains('\r'), "the CR itself is dropped: {kept:?}");
}

/// The behaviour the cut exists for: a progress bar redrawing its own row
/// leaves only what came after the last carriage return.
#[test]
fn a_true_carriage_return_still_overwrites() {
    let kept: String = sanitize_line(Line::from(Span::raw("downloading 10%\rdownloading 90%")))
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(kept, "downloading 90%");
}
