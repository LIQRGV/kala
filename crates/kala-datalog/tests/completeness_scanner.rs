//! Chunk boundaries must not change the batch lexical contract.
use kala_datalog::{Completeness, CompletenessScanner, MAX_DEPTH, MAX_QUERY_BYTES, completeness};
use proptest::prelude::*;

fn every_split(text: &str) {
    // push accepts &str, so a chunk cannot end within a UTF-8 scalar.
    // Exercise every legal boundary, including around multibyte scalars.
    for at in (0..=text.len()).filter(|&at| text.is_char_boundary(at)) {
        let mut scanner = CompletenessScanner::new();
        assert_eq!(
            scanner.push(&text[..at]),
            completeness(&text[..at]),
            "prefix at {at}: {text:?}"
        );
        assert_eq!(
            scanner.push(""),
            completeness(&text[..at]),
            "empty chunk at {at}: {text:?}"
        );
        assert_eq!(
            scanner.push(&text[at..]),
            completeness(text),
            "split at {at}: {text:?}"
        );
    }
    let mut scanner = CompletenessScanner::new();
    for (at, c) in text.char_indices() {
        let end = at + c.len_utf8();
        assert_eq!(
            scanner.push(&text[at..end]),
            completeness(&text[..end]),
            "one-character chunks at {end}: {text:?}"
        );
    }
}

#[test]
fn escape_comment_and_unicode_boundaries_match_batch() {
    for text in [
        "",
        " ; [ ignored ]\n",
        "[; ignored ] ) \r\n(\"é😀中 ] ; \\\" \\\\ \\u0041\\u03BB\") ]",
        "[\"abc\\",
        "[\"abc\\u0",
        "[\"abc\\u00g0\"]",
        "[\"\\uD800\"]",
        "[\"\\q\"]",
        "[] ; comment",
        "[] ; comment\n[",
        "[] \"unterminated",
        "[?é :名 1e+3 1.25 -1.25 TYPE#123 edge#9]",
        "[1e3]",
        "[1e999]",
        "[? : ]",
        "[)(]",
        "[] []",
        "({})",
        "λ",
    ] {
        every_split(text);
    }
}

#[test]
fn known_batch_results_do_not_change_with_the_new_scanner() {
    use Completeness::{Complete, Incomplete, Invalid};
    for (source, expected) in [
        ("", Incomplete),
        ("; comment", Incomplete),
        ("[", Incomplete),
        ("[\"a\\", Incomplete),
        ("[\"a\\u12", Incomplete),
        ("[\"a\\u12x", Invalid),
        ("[\"\\uD800", Invalid),
        ("[\"\\q", Invalid),
        ("[?", Invalid),
        ("[?x", Incomplete),
        ("[1e", Invalid),
        ("[1e3", Incomplete),
        ("[1e999", Invalid),
        ("[1.0]", Complete),
        ("[]", Complete),
        ("[] ; [ comment", Complete),
        ("[] [", Invalid),
        ("[] \"", Invalid),
        ("[)", Invalid),
        ("{", Invalid),
        (":", Invalid),
        (":name", Complete),
        ("?", Invalid),
        ("?é", Complete),
        ("1e", Invalid),
        ("1e3", Complete),
        ("1e999", Invalid),
        ("λ", Complete),
        ("😀", Invalid),
        ("\"😀\"", Complete),
    ] {
        assert_eq!(completeness(source), expected, "{source:?}");
        assert_eq!(
            CompletenessScanner::new().push(source),
            expected,
            "{source:?}"
        );
    }
}

#[test]
fn reset_clears_complete_invalid_and_partial_inputs() {
    let mut scanner = CompletenessScanner::new();
    for before in ["[]", "[)", "[\"a\\u12", "; comment", "1e"] {
        scanner.push(before);
        scanner.reset();
        assert_eq!(scanner.push(""), Completeness::Incomplete);
        assert_eq!(scanner.push("["), Completeness::Incomplete);
        assert_eq!(scanner.push("é]"), Completeness::Complete);
        scanner.reset();
    }
}

#[test]
fn size_and_depth_limits_apply_across_chunks_and_reset() {
    let mut scanner = CompletenessScanner::new();
    for _ in 0..MAX_DEPTH {
        assert_eq!(scanner.push("["), Completeness::Incomplete);
    }
    assert_eq!(scanner.push("["), Completeness::Invalid);
    scanner.reset();
    assert_eq!(
        scanner.push(&"[".repeat(MAX_DEPTH)),
        Completeness::Incomplete
    );
    assert_eq!(scanner.push(&"]".repeat(MAX_DEPTH)), Completeness::Complete);
    scanner.reset();
    assert_eq!(
        scanner.push(&" ".repeat(MAX_QUERY_BYTES - 2)),
        Completeness::Incomplete
    );
    assert_eq!(scanner.push("[]"), Completeness::Complete);
    assert_eq!(scanner.push(" "), Completeness::Invalid);
    scanner.reset();
    assert_eq!(scanner.push("[]"), Completeness::Complete);
}

fn token_soup() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(vec![
            "[", "]", "(", ")", "{", "}", " ", "\n", "\r", ",", ";", "\"", "\\", "\\u", "D800",
            "0041", "q", "é", "😀", "中", "?", ":", "x", "true", "1", ".", "e", "+", "-", "#",
            "edge#9", "\u{2003}",
        ]),
        0..40,
    )
    .prop_map(|tokens| tokens.concat())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn every_split_of_token_soup_agrees(text in token_soup()) {
        every_split(&text);
    }

    #[test]
    fn arbitrary_unicode_chunks_agree(text in ".{0,120}", chunk_sizes in prop::collection::vec(0usize..20, 1..30)) {
        let mut scanner = CompletenessScanner::new();
        let boundaries: Vec<_> = text.char_indices().map(|(i, _)| i).chain(std::iter::once(text.len())).collect();
        let mut at = 0;
        for size in chunk_sizes {
            let next = (at + size).min(boundaries.len() - 1);
            let end = boundaries[next];
            prop_assert_eq!(scanner.push(&text[boundaries[at]..end]), completeness(&text[..end]));
            at = next;
        }
        prop_assert_eq!(scanner.push(&text[boundaries[at]..]), completeness(&text));
    }
}

#[test]
fn eight_digit_unicode_escapes_preserve_incremental_completeness_and_depth_limits() {
    use Completeness::{Complete, Incomplete, Invalid};
    for (source, expected) in [
        (r#"["\U0010FFFF"]"#, Complete),
        (r#"["\U000e0001\U000e007f"]"#, Complete),
        (r#"["\U0000D800"]"#, Invalid),
        (r#"["\U00110000"]"#, Invalid),
        (r#"["\UFFFFFFFF"]"#, Invalid),
        (r#"["\U000e00g1"]"#, Invalid),
        (r#"["\U000e00"#, Incomplete),
    ] {
        assert_eq!(completeness(source), expected, "{source}");
        for split in 0..=source.len() {
            let mut scanner = CompletenessScanner::new();
            scanner.push(&source[..split]);
            assert_eq!(
                scanner.push(&source[split..]),
                expected,
                "split {split}: {source}"
            );
        }
    }
    let too_deep = format!(
        "{}\"\\U0010FFFF\"{}",
        "[".repeat(MAX_DEPTH + 1),
        "]".repeat(MAX_DEPTH + 1)
    );
    assert_eq!(completeness(&too_deep), Invalid);
}
