//! Display-only escaping for untrusted text in the terminal editor. Query
//! bytes are preserved; only rendered text is changed. Raw control/format
//! characters can have imperfect cursor placement because rustyline computes
//! layout on the original bytes; ordinary Datalog escapes keep normal layout.

use std::borrow::Cow;

use rustyline::completion::Completer;
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::Hinter;
use rustyline::validate::Validator;

use crate::escape;

pub(super) struct SafeHelper;

impl Completer for SafeHelper {
    type Candidate = String;
}
impl Hinter for SafeHelper {
    type Hint = String;
}
impl Validator for SafeHelper {}
impl rustyline::Helper for SafeHelper {}

impl Highlighter for SafeHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        Cow::Owned(escape::text(line))
    }

    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        _default: bool,
    ) -> Cow<'b, str> {
        // Reverse search includes its search text in a dynamic prompt.
        Cow::Owned(escape::text(prompt))
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Cow::Owned(escape::text(hint))
    }

    fn highlight_candidate<'c>(
        &self,
        candidate: &'c str,
        _completion: rustyline::config::CompletionType,
    ) -> Cow<'c, str> {
        Cow::Owned(escape::text(candidate))
    }

    fn highlight_char(&self, _line: &str, _pos: usize, _kind: CmdKind) -> bool {
        // Disable the optimized direct-to-terminal character write, including
        // after cursor moves and the final refresh on Enter.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustyline::config::CompletionType;

    #[test]
    fn editor_escapes_lines_search_prompts_hints_and_candidates() {
        let helper = SafeHelper;
        let source = "[:find ?x :where [?x :name \"\u{1b}]52;c;injected\u{7}\u{202e}\"]]";
        let original = source.as_bytes().to_vec();
        let shown = helper.highlight(source, source.len());
        for output in [
            shown,
            helper.highlight_prompt(source, false),
            helper.highlight_hint(source),
            helper.highlight_candidate(source, CompletionType::List),
        ] {
            assert!(!output.chars().any(kala_core::must_escape), "{output:?}");
            assert!(
                output.contains("\\u{1b}]52;c;injected\\u{7}\\u{202e}"),
                "{output:?}"
            );
        }
        assert_eq!(source.as_bytes(), original, "escaping is display-only");
    }

    #[test]
    fn ordinary_input_and_layout_are_preserved() {
        let source = "[:find ?x\n\t:where [?x :name \"\\u202e\"]]";
        assert_eq!(SafeHelper.highlight(source, 0), source);
        assert_eq!(SafeHelper.highlight_prompt("kala> ", true), "kala> ");
        assert_eq!(SafeHelper.highlight_prompt("  ...> ", true), "  ...> ");
    }

    #[test]
    fn every_editor_refresh_uses_the_escaping_path() {
        for kind in [CmdKind::MoveCursor, CmdKind::Other, CmdKind::ForcedRefresh] {
            assert!(SafeHelper.highlight_char("safe", 0, kind));
            assert!(SafeHelper.highlight_char("\u{202e}", 0, kind));
        }
    }
}
