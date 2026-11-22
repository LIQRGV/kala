use kala_datalog::{Completeness, completeness, parse};
use proptest::prelude::*;

#[test]
fn lexical_completeness_handles_brackets_strings_escapes_and_comments() {
    for source in [
        "[:find ?e :where [?e :name _]]",
        "[:find ?e :where [?e :name \"a ] ( b\"]]",
        "[:find ?e :where [?e :name \"a \\\" ] b\"]]",
        "[:find ?e ; ] ) ignored\n :where [?e :name _]]",
        "[:find ?e :where (not [?e :name \"x\"])]",
    ] {
        assert_eq!(completeness(source), Completeness::Complete, "{source}");
    }
    for source in [
        "",
        " \n; comment [\n",
        "[",
        "[:find ?e",
        "[\"abc",
        "[\"abc\\",
        "[(",
    ] {
        assert_eq!(completeness(source), Completeness::Incomplete, "{source:?}");
    }
    for source in ["]", "[)", "([]]", "[\"bad\\q\"]", "[] \""] {
        assert_eq!(completeness(source), Completeness::Invalid, "{source}");
    }
}

proptest! {
    #[test]
    fn completeness_never_panics_or_calls_unexpected_eof_complete(s in ".{0,512}") {
        if completeness(&s) == Completeness::Complete && let Err(err) = parse(&s) {
                let message = err.to_string().to_lowercase();
                prop_assert!(!message.contains("unexpected eof") && !message.contains("unexpected end"), "{s:?}: {message}");
        }
    }
}
