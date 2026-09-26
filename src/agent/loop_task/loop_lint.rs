//! Claim lint: post-generation check of result claims (moved byte-for-byte
//! from the parent module; behavior unchanged).


/// Claim lint (Y, §12.9): post-generation check of result claims in the
/// final answer against this turn's journal window (records after the
/// latest user message). Only CONTRADICTIONS are marked — absence of
/// evidence is silence, never a flag. Never fails and never blocks:
/// any internal error returns the text untouched.
pub(crate) fn lint_answer(
    text: &str,
    root: &std::path::Path,
    session_id: &str,
    journal: &mut Option<crate::agent::journal::Journal>,
) -> String {
    // quoting the user is not claiming: drop markdown-quote lines first
    let visible: String = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('>'))
        .collect::<Vec<_>>()
        .join("\n");
    let records = crate::agent::journal::Journal::records_for(root, session_id).unwrap_or_default();
    let start = records
        .iter()
        .filter(|r| r.kind == "user_msg")
        .map(|r| r.seq)
        .max()
        .unwrap_or(0);
    let results: Vec<(String, bool, String)> = records
        .iter()
        .filter(|r| r.kind == "tool_result" && r.seq > start)
        .map(|r| {
            (
                r.fields
                    .get("tool")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                r.fields
                    .get("ok")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                r.fields
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect();
    let any_fail = results.iter().any(|(_, ok, _)| !ok);
    let exec_ok = results.iter().any(|(tool, ok, _)| *ok && tool == "bash");
    let summaries = results
        .iter()
        .map(|(_, _, s)| s.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    // candidate spans in first-seen order; each distinct span is marked once
    let mut spans: Vec<(&str, &str)> = Vec::new(); // (kind, span)
    for (kind, span) in extract_counts(&visible) {
        // an x/y shorthand ("10808/10809") is verified when both halves
        // were reported separately: the journal carries facts, not the
        // model's punctuation
        let verified = summaries.contains(span) || count_parts_verified(&summaries, span);
        if !verified && any_fail {
            push_span(&mut spans, kind, span);
        }
    }
    // sized claims near result words verify the same way: a number the
    // journal never reported beside tests/build/exit is a claim, whatever
    // language it wears.
    for (kind, span) in extract_sized_claims(&visible) {
        let verified = summaries.contains(span);
        if !verified && any_fail {
            push_span(&mut spans, kind, span);
        }
    }
    for (_, span) in extract_status_words(&visible) {
        if exec_ok {
            continue;
        }
        if any_fail {
            push_span(&mut spans, "status", span);
        }
    }
    for span in extract_paths(&visible) {
        if path_deleted_nearby(&visible, span) {
            continue;
        }
        // a path followed by an arrow ("services.msc -> its publisher") is
        // a usage pointer — advice to open something — not an existence
        // claim about the project tree
        if path_arrow_after(&visible, span) {
            continue;
        }
        if !path_exists(root, span) {
            push_span(&mut spans, "path", span);
        }
    }
    // symbols last and fewest: each costs a graph lookup
    if spans.len() < 40
        && let Ok(mut store) = crate::agent::graph::SqliteGraphStore::open(root)
    {
        for sym in extract_symbols(&visible) {
            if spans.len() >= 40 {
                break;
            }
            match store.resolve_ref(None, None, Some(sym)) {
                Ok(crate::agent::graph::ResolveRefResult::NotFound { .. }) => {
                    push_span(&mut spans, "symbol", sym)
                }
                Ok(_) => {}
                Err(_) => {} // infra failure: silence, not a verdict
            }
        }
    }
    if spans.is_empty() {
        return text.to_string();
    }
    if let Some(writer) = journal.as_mut() {
        for (kind, span) in &spans {
            let _ = writer.append(
                "claim_lint",
                serde_json::json!({
                    "span": span,
                    "kind": kind,
                    "by": "host",
                }),
            );
        }
    }
    // mark first occurrence of each span; offsets shift as we insert
    let mut marked = text.to_string();
    let mut done: Vec<&str> = Vec::new();
    for (_, span) in &spans {
        if done.contains(span) {
            continue;
        }
        done.push(span);
        if let Some(pos) = marked.find(span) {
            marked.insert_str(pos + span.len(), " [unverified]");
        }
    }
    marked
}

/// Capped dedup push for lint spans. A span already covered by a longer
/// collected one (e.g. "tests pass" inside "All tests pass") is skipped.
fn push_span<'x>(spans: &mut Vec<(&'static str, &'x str)>, kind: &'static str, span: &'x str) {
    if spans.len() < 40 && !spans.iter().any(|(_, s)| *s == span || s.contains(span)) {
        spans.push((kind, span));
    }
}

/// True when the byte before `pos` continues the same token: a digit span
/// starting right after a letter, dot, colon or slash is the tail of an
/// address, version or path ("127.0.0.1:10808", "v2.1"), not a count of
/// its own. Callers pass token starts, which are char boundaries.
fn token_char_before(text: &str, pos: usize) -> bool {
    text[..pos]
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric() || ".:/".contains(c))
}

/// `12 passed`, `3 failed`, `280/280` — byte spans into `text`.
/// Char-walked: every index below is a char boundary by construction
/// (byte-walking multibyte text panicked here on Cyrillic input).
pub(crate) fn extract_counts(text: &str) -> Vec<(&'static str, &str)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        let c = text[i..].chars().next().unwrap_or('\0');
        if !c.is_ascii_digit() {
            i += c.len_utf8();
            continue;
        }
        let start = i;
        while i < text.len() && text[i..].chars().next().is_some_and(|c| c.is_ascii_digit()) {
            i += 1;
        }
        let mut j = i;
        while j < text.len() && text[j..].chars().next().is_some_and(|c| c.is_whitespace()) {
            j += text[j..].chars().next().unwrap().len_utf8();
        }
        // x/y form — but only standalone: "280/280" is a count, while
        // "127.0.0.1:10808/10809" is an address and "v2.1/3" a version.
        // A token char immediately before the first digit means the span is
        // the tail of something bigger, not a claim of its own.
        if text[j..].starts_with('/') && !token_char_before(text, start) {
            let mut k = j + 1;
            while k < text.len() && text[k..].chars().next().is_some_and(|c| c.is_ascii_digit()) {
                k += 1;
            }
            if k > j + 1 {
                out.push(("count", &text[start..k]));
                i = k;
                continue;
            }
        }
        // word form: passed|failed
        let mut k = j;
        while k < text.len()
            && text[k..]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric())
        {
            k += text[k..].chars().next().unwrap().len_utf8();
        }
        if &text[j..k] == "passed" || &text[j..k] == "failed" {
            out.push(("count", &text[start..k]));
            i = k;
        }
    }
    out
}

/// Sized claims near result words: "12 tests", "tests: 12", "exit 0",
/// "exit code 0", "0 errors", "12 ошибок" — English and Russian. A bare
/// number elsewhere ("3 files") is not a result claim. Spans run across
/// the number and the word (either order), allowing whitespace and
/// `:`, `#`, `,` between them. Verified against tool summaries exactly
/// like `extract_counts`.
fn extract_sized_claims(text: &str) -> Vec<(&'static str, &str)> {
    const WORDS: &[&str] = &[
        "test", "tests", "build", "exit", "error", "errors", "тест", "тесты", "тестов",
        "сборка", "сборки", "ошибка", "ошибки", "ошибок",
    ];
    // alphanumeric tokens with byte spans; every index below stays a char
    // boundary by construction (byte-walking multibyte text panics).
    let mut toks: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < text.len() {
        let c = text[i..].chars().next().unwrap();
        if c.is_alphanumeric() {
            let start = i;
            while i < text.len()
                && text[i..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric())
            {
                i += text[i..].chars().next().unwrap().len_utf8();
            }
            toks.push((start, i));
        } else {
            i += c.len_utf8();
        }
    }
    let word_at = |k: usize| toks.get(k).map(|(s, e)| &text[*s..*e]);
    let is_num =
        |k: usize| word_at(k).is_some_and(|w| !w.is_empty() && w.chars().all(|c| c.is_ascii_digit()));
    let is_word =
        |k: usize| word_at(k).is_some_and(|w| WORDS.contains(&w.to_lowercase().as_str()));
    let gap_ok = |a_end: usize, b_start: usize| {
        text[a_end..b_start]
            .chars()
            .all(|c| c.is_whitespace() || ":,#№".contains(c))
    };
    let mut out = Vec::new();
    let mut k = 0;
    while k < toks.len() {
        // "exit code N" triple
        if k + 2 < toks.len()
            && word_at(k).is_some_and(|w| w.to_lowercase() == "exit")
            && word_at(k + 1).is_some_and(|w| w.to_lowercase() == "code")
            && is_num(k + 2)
            && gap_ok(toks[k].1, toks[k + 1].0)
            && gap_ok(toks[k + 1].1, toks[k + 2].0)
        {
            out.push(("count", &text[toks[k].0..toks[k + 2].1]));
            k += 3;
            continue;
        }
        // "N word" and "word N" pairs
        if k + 1 < toks.len()
            && gap_ok(toks[k].1, toks[k + 1].0)
            && ((is_num(k) && is_word(k + 1)) || (is_word(k) && is_num(k + 1)))
        {
            out.push(("count", &text[toks[k].0..toks[k + 1].1]));
            k += 2;
            continue;
        }
        k += 1;
    }
    out
}

/// status phrases that assert success without numbers. English plus
/// Russian: the model often answers in Russian, and a warn-layer that
/// only reads English is blind to half the claims. Conservative list —
/// success-asserting phrases only, since every hit marks text.
pub(crate) fn extract_status_words(text: &str) -> Vec<(&'static str, &str)> {    const PHRASES: &[&str] = &[
        "build succeeded",
        "builds succeeded",
        "all green",
        "tests pass",
        "test passes",
        "suite passes",
        "suites pass",
        "suite green",
        "all tests pass",
        "everything passes",
        "тесты прошли",
        "тест прошел",
        "тест прошёл",
        "все тесты прошли",
        "тесты зеленые",
        "тесты зелёные",
        "все зеленые",
        "все зелёные",
        "всё зелёное",
        "все зелено",
        "сборка прошла",
        "сборка успешна",
        "успешно собралось",
        "собралось",
        "исправлено",
        "баг исправлен",
        "ошибка исправлена",
    ];
    let lower = text.to_lowercase();
    let mut out = Vec::new();
    for phrase in PHRASES {
        // first occurrence span mapped back by byte search (phrases are ASCII).
        // `find` runs on the lowered copy, whose byte coordinates can drift
        // from the original when case-folding changes length — so the slice
        // is re-verified, not just boundary-checked.
        if let Some(pos) = lower.find(phrase) {
            let end = pos + phrase.len();
            if text.is_char_boundary(pos)
                && text.is_char_boundary(end)
                && text[pos..end].to_lowercase() == *phrase
            {
                out.push(("status", &text[pos..end]));
            }
        }
    }
    out
}

/// path-looking tokens: contain `/` and `.`, or backticked with a dot.
/// Trailing punctuation stripped. Returns spans into `text`.
/// Char-walked with a boundary invariant on every index (the byte version
/// panicked on multibyte input: a Cyrillic lead byte reads as alphanumeric
/// and stops the scan mid-char).
pub(crate) fn extract_paths(text: &str) -> Vec<&str> {
    fn is_tok(c: char) -> bool {
        c.is_alphanumeric() || "._-/".contains(c)
    }
    let mut out = Vec::new();
    let mut i = 0;
    // invariant: i, start and end below are always char boundaries
    while i < text.len() {
        let c = text[i..].chars().next().unwrap();
        let backticked = c == '`';
        if backticked {
            i += 1;
        } else if !is_tok(c) {
            i += c.len_utf8();
            continue;
        }
        let start = i;
        while i < text.len() {
            let c = text[i..].chars().next().unwrap();
            if !is_tok(c) {
                break;
            }
            i += c.len_utf8();
        }
        let mut end = i;
        while end > start {
            // end starts at a boundary and moves back by whole chars
            match text[..end].chars().next_back() {
                Some(c) if ",.:;!?".contains(c) => end -= c.len_utf8(),
                _ => break,
            }
        }
        let closed = backticked && text[end..].starts_with('`');
        if end > start {
            let tok = &text[start..end];
            if (tok.contains('/') && tok.contains('.'))
                || (backticked && closed && tok.contains('.'))
            {
                out.push(tok);
            }
        }
        if backticked && text[i..].starts_with('`') {
            i += 1; // skip the closing backtick so it is not rescanned
        }
    }
    out
}

/// guard for tombstone claims ("deleted x.rs"): a deletion verb in the
/// preceding 24 chars means a missing file is expected, not a lie.
pub(crate) fn path_deleted_nearby(text: &str, span: &str) -> bool {
    const VERBS: &[&str] = &[
        "delete",
        "deleted",
        "remove",
        "removed",
        "rm ",
        "unlink",
        "deleting",
        "removing",
        "удалил",
        "удали",
        "удалить",
        "удалено",
        "убрал",
        "стёр",
        "стер",
    ];
    let Some(pos) = text.find(span) else {
        return false;
    };
    // `from` is a raw byte rewind and can land mid-char on multibyte text;
    // walk forward to a boundary (keeps the ~48-byte window, never panics).
    let mut from = pos.saturating_sub(48);
    while !text.is_char_boundary(from) {
        from += 1;
    }
    let before = text[from..pos].to_lowercase();
    VERBS.iter().any(|v| before.contains(v))
}

/// An x/y count ("10808/10809") is verified when both halves were reported
/// separately: the journal carries facts, not the model's punctuation.
/// Plain numbers only — anything else falls back to exact matching.
fn count_parts_verified(summaries: &str, span: &str) -> bool {
    let mut halves = span.split('/');
    let (Some(a), Some(b), None) = (halves.next(), halves.next(), halves.next()) else {
        return false;
    };
    let (a, b) = (a.trim(), b.trim());
    !a.is_empty()
        && !b.is_empty()
        && a.chars().all(|c| c.is_ascii_digit())
        && b.chars().all(|c| c.is_ascii_digit())
        && summaries.contains(a)
        && summaries.contains(b)
}

/// A path followed by an arrow ("services.msc -> its publisher") is a usage
/// pointer, not an existence claim. Checks the text right after the span's
/// first occurrence (past a closing backtick, if the span was quoted).
fn path_arrow_after(text: &str, span: &str) -> bool {
    let Some(pos) = text.find(span) else {
        return false;
    };
    let rest = text[pos + span.len()..]
        .trim_start()
        .trim_start_matches(['`', '"', '\''])
        .trim_start();
    rest.starts_with("->") || rest.starts_with('→')
}

fn path_exists(root: &std::path::Path, span: &str) -> bool {
    let rel = std::path::Path::new(span);
    if rel.is_absolute() {
        return rel.exists();
    }
    root.join(rel).exists()
}

/// `path::symbol` tokens. Returns spans into `text`, capped by the caller.
pub(crate) fn extract_symbols(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for tok in text.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':' || c == '/')) {
        if tok.contains("::") && !tok.starts_with(':') && !tok.ends_with(':') {
            out.push(tok);
        }
    }
    out
}

