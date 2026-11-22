//! Slice 4, item 2: the case-insensitive V index (docs/slice-4-decisions.md
//! "Item 2" and "Dependencies"; CLAUDE.md "Value types", strings). RED until
//! the implementation lands: this file fails to COMPILE.
//!
//! API expected (all re-exported at the crate root):
//!
//! ```text
//! pub fn fold_for_search(s: &str) -> String          // module value
//!     // NFC(full default case fold(NFD(s))), no Turkic mappings
//! World::lookup_ci(&self, attr: &str, text: &str, t: Tick) -> Result<Vec<EntityId>, Error>
//! View::lookup_ci (same signature, at the pinned canon)
//! IndexName::VFold, IndexName::VFoldLatest
//! IndexDump { .., pub v_fold: Vec<(AttrId, String, Value, EntityId, Tick, TxId, Mark)> }
//!     // (attr, folded, original, entity, tick, tx, mark), key order
//! ```
//!
//! Contract: `lookup_ci(attr, text, t)` = every entity (or edge) with ANY
//! value of `attr` holding at `t` whose fold equals `fold_for_search(text)`;
//! ascending by EntityId, no duplicates. The folded key also carries the
//! original value, so two values that fold equal keep separate histories.
//! Errors: unknown attribute or inverse name -> UnknownAttribute; unindexed ->
//! NotIndexed; non-string attribute -> WrongValueType { expected: attr type,
//! got: Str }. Any length of query text is Ok.

use kala_core::*;
use proptest::prelude::*;
use proptest::test_runner::{RngAlgorithm, RngSeed};

// ---------------------------------------------------------------- helpers

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "title",
            ValueType::Str,
            Kind::Event,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new("bio", ValueType::Str, Kind::State, Cardinality::One).unindexed())
        .attribute(AttrDef::new(
            "rank",
            ValueType::I64,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "alive",
            ValueType::Bool,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "owns",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "present",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(
            AttrDef::new("parent_of", ValueType::Ref, Kind::State, Cardinality::Many)
                .inverse("child_of"),
        )
        .attribute(AttrDef::new(
            "note",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
}

#[derive(Clone, Copy)]
struct Ents {
    edwin: EntityId,
    greg: EntityId,
    mara: EntityId,
    varrow: EntityId,
}

fn populate(w: &mut World) -> Ents {
    Ents {
        edwin: w.create_entity("CHARACTER", Some("edwin")).unwrap(),
        greg: w.create_entity("CHARACTER", Some("greg")).unwrap(),
        mara: w.create_entity("CHARACTER", Some("mara")).unwrap(),
        varrow: w.create_entity("PLACE", Some("varrow")).unwrap(),
    }
}

fn mem() -> (World, Ents) {
    let mut w = World::in_memory(schema()).unwrap();
    let e = populate(&mut w);
    (w, e)
}

fn ci(w: &World, attr: &str, text: &str, at: i64) -> Vec<EntityId> {
    w.lookup_ci(attr, text, Tick(at)).unwrap()
}

fn only(r: WriteResult) -> EntryId {
    assert_eq!(r.entries.len(), 1);
    r.entries[0]
}

fn rep(value: Option<Value>, tick: Option<Tick>, op: Option<Op>) -> Correction {
    Correction::Replace {
        value,
        tick,
        op,
        precision: None,
    }
}

fn clean(w: &World) {
    let rep = doctor(w, &DoctorOptions::default());
    assert!(rep.is_clean(), "{:#?}", rep.findings);
}

/// `at_canon` (latest-revision path at the latest canon) and `at_canon_main`
/// (main index with the canon skip) agree.
fn both_paths(w: &World, tx: TxId, attr: &str, text: &str, at: i64) -> Vec<EntityId> {
    let a = w.at_canon(tx).lookup_ci(attr, text, Tick(at)).unwrap();
    let b = w.at_canon_main(tx).lookup_ci(attr, text, Tick(at)).unwrap();
    assert_eq!(
        a, b,
        "at_canon vs at_canon_main, {tx:?} {attr}={text:?} @{at}"
    );
    a
}

// ---------------------------------------------------------------- fold_for_search

#[test]
fn fold_makes_ascii_case_irrelevant() {
    assert_eq!(fold_for_search("Edwin"), "edwin");
    assert_eq!(fold_for_search("EDWIN"), "edwin");
    assert_eq!(fold_for_search("edwin"), "edwin");
    assert_eq!(fold_for_search("eDwIn"), "edwin");
    assert_eq!(fold_for_search(""), "");
    assert_eq!(fold_for_search("123 -_ !"), "123 -_ !");
}

#[test]
fn fold_treats_canonically_equivalent_forms_as_equal() {
    let precomposed = "\u{00C9}"; // É
    let decomposed = "E\u{0301}";
    assert_eq!(fold_for_search(precomposed), fold_for_search(decomposed));
    // the result is NFC: the precomposed lower-case e-acute
    assert_eq!(fold_for_search(decomposed), "\u{00E9}");
    assert_eq!(fold_for_search("\u{00E9}"), "\u{00E9}");
    assert_eq!(fold_for_search("e\u{0301}"), "\u{00E9}");
    // a longer mixed word
    assert_eq!(
        fold_for_search("E\u{0301}MILE"),
        fold_for_search("\u{00C9}mile")
    );
}

#[test]
fn fold_is_full_case_folding_sharp_s_becomes_ss() {
    assert_eq!(fold_for_search("Stra\u{00DF}e"), "strasse");
    assert_eq!(fold_for_search("STRASSE"), "strasse");
    assert_eq!(fold_for_search("\u{1E9E}"), "ss"); // capital sharp s
}

#[test]
fn fold_greek_final_sigma_equals_sigma() {
    assert_eq!(fold_for_search("\u{03C2}"), "\u{03C3}"); // ς -> σ
    assert_eq!(fold_for_search("\u{03A3}"), "\u{03C3}"); // Σ -> σ
    assert_eq!(fold_for_search("\u{03C3}"), "\u{03C3}");
    assert_eq!(
        fold_for_search("\u{039F}\u{0394}\u{03A5}\u{03A3}\u{03A3}\u{0395}\u{03A5}\u{03A3}"), // ΟΔΥΣΣΕΥΣ
        fold_for_search("\u{03BF}\u{03B4}\u{03C5}\u{03C3}\u{03C3}\u{03B5}\u{03C5}\u{03C2}"), // οδυσσευς
    );
}

#[test]
fn fold_uses_default_rules_not_turkic() {
    // U+0130 (I with dot above) full-folds to "i" + U+0307, never plain "i"
    assert_eq!(fold_for_search("\u{0130}"), "i\u{0307}");
    // plain "I" folds to "i" (Turkic would give dotless U+0131)
    assert_eq!(fold_for_search("I"), "i");
    assert_eq!(fold_for_search("\u{0131}"), "\u{0131}"); // dotless i is left alone
}

#[test]
fn fold_can_be_longer_than_its_input() {
    // U+0149 (2 bytes) folds to U+02BC + "n" (3 bytes)
    assert_eq!(fold_for_search("\u{0149}"), "\u{02BC}n");
    assert_eq!("\u{0149}".len(), 2);
    assert_eq!(fold_for_search("\u{0149}").len(), 3);
    // the ffi ligature folds to ascii
    assert_eq!(fold_for_search("\u{FB03}"), "ffi");
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        rng_algorithm: RngAlgorithm::ChaCha,
        rng_seed: RngSeed::Fixed(0x666f_6c64),
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn fold_is_idempotent(text in "\\PC{0,40}") {
        let once = fold_for_search(&text);
        prop_assert_eq!(fold_for_search(&once), once);
    }

    #[test]
    fn fold_is_idempotent_on_any_string(text in any::<String>()) {
        let once = fold_for_search(&text);
        prop_assert_eq!(fold_for_search(&once), once);
    }

    #[test]
    fn upper_and_lower_case_of_the_same_ascii_word_fold_equal(text in "[A-Za-z0-9 ]{0,24}") {
        prop_assert_eq!(
            fold_for_search(&text.to_uppercase()),
            fold_for_search(&text.to_lowercase())
        );
    }
}

// ---------------------------------------------------------------- lookup_ci basics

#[test]
fn finds_edwin_in_any_case_and_any_normalisation() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "name", s("Edwin"), t(10)).unwrap();
    w.assert(e.greg, "name", s("E\u{0301}mile"), t(10)).unwrap(); // decomposed
    w.assert(e.mara, "name", s("\u{00C9}MILE"), t(10)).unwrap(); // precomposed
    for q in ["Edwin", "EDWIN", "edwin", "eDWIN"] {
        assert_eq!(ci(&w, "name", q, 10), vec![e.edwin], "{q}");
    }
    for q in [
        "\u{00E9}mile",
        "E\u{0301}MILE",
        "\u{00C9}mile",
        "e\u{0301}mile",
    ] {
        assert_eq!(ci(&w, "name", q, 10), vec![e.greg, e.mara], "{q:?}");
    }
    assert_eq!(ci(&w, "name", "Edwi", 10), vec![]);
    assert_eq!(ci(&w, "name", "Edwin ", 10), vec![]);
    assert_eq!(ci(&w, "name", "", 10), vec![]);
    clean(&w);
}

#[test]
fn finds_sharp_s_through_ss_and_sigma_through_either_form() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "name", s("Stra\u{00DF}e"), t(0)).unwrap();
    w.assert(
        e.greg,
        "name",
        s("\u{039F}\u{0394}\u{03A5}\u{03A3}\u{03A3}\u{0395}\u{03A5}\u{03A3}"),
        t(0),
    )
    .unwrap();
    assert_eq!(ci(&w, "name", "STRASSE", 0), vec![e.edwin]);
    assert_eq!(ci(&w, "name", "strasse", 0), vec![e.edwin]);
    assert_eq!(ci(&w, "name", "STRA\u{1E9E}E", 0), vec![e.edwin]);
    assert_eq!(
        ci(
            &w,
            "name",
            "\u{03BF}\u{03B4}\u{03C5}\u{03C3}\u{03C3}\u{03B5}\u{03C5}\u{03C2}",
            0
        ),
        vec![e.greg]
    );
    clean(&w);
}

#[test]
fn turkic_dotted_capital_i_does_not_match_plain_i() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "name", s("\u{0130}stanbul"), t(0))
        .unwrap();
    w.assert(e.greg, "name", s("istanbul"), t(0)).unwrap();
    assert_eq!(ci(&w, "name", "i\u{0307}stanbul", 0), vec![e.edwin]);
    assert_eq!(ci(&w, "name", "\u{0130}STANBUL", 0), vec![e.edwin]);
    assert_eq!(ci(&w, "name", "ISTANBUL", 0), vec![e.greg]);
}

#[test]
fn results_are_sorted_by_entity_id_without_duplicates() {
    let (mut w, e) = mem();
    // written in reverse id order, and one entity holds two values folding equal
    w.assert(e.mara, "name", s("Sam"), t(0)).unwrap();
    w.assert(e.greg, "name", s("SAM"), t(0)).unwrap();
    w.assert(e.edwin, "name", s("sam"), t(0)).unwrap();
    w.assert(e.edwin, "name", s("Sam"), t(0)).unwrap();
    assert_eq!(ci(&w, "name", "sAm", 0), vec![e.edwin, e.greg, e.mara]);
}

#[test]
fn ticks_before_the_first_fact_and_extremes() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "name", s("Early"), t(i64::MIN)).unwrap();
    w.assert(e.greg, "name", s("Late"), t(i64::MAX)).unwrap();
    assert_eq!(ci(&w, "name", "EARLY", i64::MIN), vec![e.edwin]);
    assert_eq!(ci(&w, "name", "EARLY", i64::MAX), vec![e.edwin]);
    assert_eq!(ci(&w, "name", "LATE", i64::MAX - 1), vec![]);
    assert_eq!(ci(&w, "name", "LATE", i64::MAX), vec![e.greg]);
    w.retract(e.edwin, "name", s("Early"), t(0)).unwrap();
    assert_eq!(ci(&w, "name", "early", -1), vec![e.edwin]);
    assert_eq!(ci(&w, "name", "early", 0), vec![]);
    clean(&w);
}

#[test]
fn an_event_attribute_holds_from_its_tick_on() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "title", s("Grey King"), t(5)).unwrap();
    assert_eq!(ci(&w, "title", "GREY KING", 4), vec![]);
    assert_eq!(ci(&w, "title", "GREY KING", 5), vec![e.edwin]);
    assert_eq!(ci(&w, "title", "grey king", 1_000), vec![e.edwin]);
}

#[test]
fn an_edge_property_returns_the_edges_entity_id() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "present", Value::Ref(e.varrow), t(0))
        .unwrap();
    let edge = EntityId::from(w.edge_id(e.edwin, "present", e.varrow).unwrap());
    w.assert(edge, "note", s("In Disguise"), t(0)).unwrap();
    w.assert(e.greg, "note", s("in disguise"), t(0)).unwrap();
    let got = ci(&w, "note", "IN DISGUISE", 0);
    assert_eq!(got.len(), 2);
    assert!(got.contains(&edge) && got.contains(&e.greg));
    let mut sorted = got.clone();
    sorted.sort();
    assert_eq!(got, sorted);
    clean(&w);
}

// ---------------------------------------------------------------- separate histories

#[test]
fn two_values_of_one_entity_that_fold_equal_keep_separate_histories() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "name", s("Edwin"), t(0)).unwrap();
    w.assert(e.edwin, "name", s("EDWIN"), t(0)).unwrap();
    w.retract(e.edwin, "name", s("EDWIN"), t(10)).unwrap();
    // "Edwin" still holds, so the entity is found at every tick from 0 on
    for at in [0, 5, 10, 11, 1_000] {
        assert_eq!(ci(&w, "name", "edwin", at), vec![e.edwin], "@{at}");
    }
    // now "Edwin" ends too: nothing holds from 20
    w.retract(e.edwin, "name", s("Edwin"), t(20)).unwrap();
    assert_eq!(ci(&w, "name", "edwin", 19), vec![e.edwin]);
    assert_eq!(ci(&w, "name", "edwin", 20), vec![]);
    // exact lookups see the two values separately
    assert_eq!(
        w.lookup("name", &s("EDWIN"), Tick(5)).unwrap(),
        vec![e.edwin]
    );
    assert_eq!(w.lookup("name", &s("EDWIN"), Tick(10)).unwrap(), vec![]);
    assert_eq!(
        w.lookup("name", &s("Edwin"), Tick(19)).unwrap(),
        vec![e.edwin]
    );
    // each original has its own folded rows
    let d = w.index_dump();
    let mut originals: Vec<(String, Vec<u8>)> = d
        .v_fold
        .iter()
        .map(|r| (r.1.clone(), r.2.encode().unwrap()))
        .collect();
    originals.sort();
    originals.dedup();
    let mut expected: Vec<(String, Vec<u8>)> = vec![
        ("edwin".to_string(), s("EDWIN").encode().unwrap()),
        ("edwin".to_string(), s("Edwin").encode().unwrap()),
    ];
    expected.sort();
    assert_eq!(originals, expected);
    clean(&w);
}

#[test]
fn not_found_when_neither_of_two_fold_equal_values_holds() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "name", s("Edwin"), t(0)).unwrap();
    w.assert(e.edwin, "name", s("EDWIN"), t(0)).unwrap();
    w.retract(e.edwin, "name", s("Edwin"), t(5)).unwrap();
    w.retract(e.edwin, "name", s("EDWIN"), t(7)).unwrap();
    assert_eq!(ci(&w, "name", "EdWiN", 4), vec![e.edwin]);
    assert_eq!(ci(&w, "name", "EdWiN", 6), vec![e.edwin]);
    assert_eq!(ci(&w, "name", "EdWiN", 7), vec![]);
    assert_eq!(ci(&w, "name", "EdWiN", 100), vec![]);
}

#[test]
fn retract_then_reassert_brings_it_back() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "name", s("Edwin"), t(0)).unwrap();
    w.retract(e.edwin, "name", s("Edwin"), t(10)).unwrap();
    w.assert(e.edwin, "name", s("Edwin"), t(20)).unwrap();
    assert_eq!(ci(&w, "name", "EDWIN", 9), vec![e.edwin]);
    assert_eq!(ci(&w, "name", "EDWIN", 10), vec![]);
    assert_eq!(ci(&w, "name", "EDWIN", 19), vec![]);
    assert_eq!(ci(&w, "name", "EDWIN", 20), vec![e.edwin]);
    clean(&w);
}

// ---------------------------------------------------------------- corrections

#[test]
fn replace_value_moves_the_folded_entry() {
    let (mut w, e) = mem();
    let entry = only(w.assert(e.edwin, "name", s("Edwin"), t(10)).unwrap());
    let tx_write = w.latest_tx();
    w.correct(entry, rep(Some(s("Mara")), None, None)).unwrap();
    let tx_rep = w.latest_tx();
    assert_eq!(ci(&w, "name", "edwin", 10), vec![]);
    assert_eq!(ci(&w, "name", "MARA", 10), vec![e.edwin]);
    assert_eq!(ci(&w, "name", "MARA", 9), vec![]);
    // latest and main paths agree at the latest canon
    assert_eq!(both_paths(&w, tx_rep, "name", "edwin", 10), vec![]);
    assert_eq!(both_paths(&w, tx_rep, "name", "MARA", 10), vec![e.edwin]);
    // the older canon still sees the old value, on both paths
    assert_eq!(both_paths(&w, tx_write, "name", "edwin", 10), vec![e.edwin]);
    assert_eq!(both_paths(&w, tx_write, "name", "MARA", 10), vec![]);
    assert_eq!(
        both_paths(&w, TxId(tx_write.0 - 1), "name", "edwin", 10),
        vec![]
    );
    clean(&w);
}

#[test]
fn replace_with_a_value_that_only_changes_case_keeps_the_entity_found() {
    let (mut w, e) = mem();
    let entry = only(w.assert(e.edwin, "name", s("Edwin"), t(10)).unwrap());
    let tx_write = w.latest_tx();
    w.correct(entry, rep(Some(s("EDWIN")), None, None)).unwrap();
    let tx_rep = w.latest_tx();
    assert_eq!(ci(&w, "name", "edwin", 10), vec![e.edwin]);
    assert_eq!(w.lookup("name", &s("Edwin"), Tick(10)).unwrap(), vec![]);
    assert_eq!(
        w.lookup("name", &s("EDWIN"), Tick(10)).unwrap(),
        vec![e.edwin]
    );
    assert_eq!(both_paths(&w, tx_rep, "name", "edwin", 10), vec![e.edwin]);
    assert_eq!(both_paths(&w, tx_write, "name", "edwin", 10), vec![e.edwin]);
    // the stored folded rows: one live original (EDWIN) at the latest canon
    clean(&w);
}

#[test]
fn replace_tick_moves_when_the_folded_value_starts_holding() {
    let (mut w, e) = mem();
    let entry = only(w.assert(e.edwin, "name", s("Edwin"), t(10)).unwrap());
    let tx_write = w.latest_tx();
    w.correct(entry, rep(None, t(-5), None)).unwrap();
    let tx_rep = w.latest_tx();
    assert_eq!(ci(&w, "name", "EDWIN", -6), vec![]);
    assert_eq!(ci(&w, "name", "EDWIN", -5), vec![e.edwin]);
    assert_eq!(both_paths(&w, tx_rep, "name", "EDWIN", -5), vec![e.edwin]);
    assert_eq!(both_paths(&w, tx_write, "name", "EDWIN", -5), vec![]);
    assert_eq!(both_paths(&w, tx_write, "name", "EDWIN", 10), vec![e.edwin]);
    clean(&w);
}

#[test]
fn delete_removes_the_entity_from_the_latest_canon_only() {
    let (mut w, e) = mem();
    let entry = only(w.assert(e.edwin, "name", s("Edwin"), t(10)).unwrap());
    let tx_write = w.latest_tx();
    w.correct(entry, Correction::Delete).unwrap();
    let tx_del = w.latest_tx();
    assert_eq!(ci(&w, "name", "EDWIN", 10), vec![]);
    assert_eq!(both_paths(&w, tx_del, "name", "EDWIN", 10), vec![]);
    assert_eq!(both_paths(&w, tx_write, "name", "EDWIN", 10), vec![e.edwin]);
    clean(&w);
}

#[test]
fn replace_op_only_flips_the_folded_row_too() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "name", s("Edwin"), t(1)).unwrap();
    let entry = only(w.assert(e.greg, "name", s("Greg"), t(5)).unwrap());
    let before = w.latest_tx();
    w.correct(entry, rep(None, None, Some(Op::Retract)))
        .unwrap();
    assert_eq!(ci(&w, "name", "GREG", 5), vec![]);
    assert_eq!(both_paths(&w, before, "name", "GREG", 5), vec![e.greg]);
    assert_eq!(both_paths(&w, w.latest_tx(), "name", "GREG", 5), vec![]);
    clean(&w);
}

// ---------------------------------------------------------------- long folds

#[test]
fn a_256_byte_value_whose_fold_is_longer_than_256_bytes_is_stored_and_found() {
    let (mut w, e) = mem();
    let original = "\u{0149}".repeat(128); // 256 bytes
    assert_eq!(original.len(), 256);
    let folded = fold_for_search(&original);
    assert_eq!(folded.len(), 384);
    w.assert(e.edwin, "name", Value::Str(original.clone()), t(0))
        .unwrap();
    assert_eq!(ci(&w, "name", &original, 0), vec![e.edwin]);
    assert_eq!(ci(&w, "name", &folded, 0), vec![e.edwin]);
    assert_eq!(
        w.lookup("name", &Value::Str(original.clone()), Tick(0))
            .unwrap(),
        vec![e.edwin]
    );
    // the folded row is in the dump with the full fold
    let d = w.index_dump();
    assert!(
        d.v_fold
            .iter()
            .any(|r| r.1 == folded && r.2 == Value::Str(original.clone()))
    );
    // and a value of 257 bytes is still refused for an indexed attribute
    let too_long = "\u{0149}".repeat(128) + "a";
    assert!(
        w.assert(e.greg, "name", Value::Str(too_long), t(0))
            .is_err()
    );
    clean(&w);
}

#[test]
fn a_ligature_value_is_found_by_its_expansion() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "name", s("O\u{FB03}ce"), t(0)).unwrap();
    assert_eq!(ci(&w, "name", "OFFICE", 0), vec![e.edwin]);
    assert_eq!(ci(&w, "name", "o\u{FB03}ce", 0), vec![e.edwin]);
}

#[test]
fn query_text_of_any_length_is_ok() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "name", s("Edwin"), t(0)).unwrap();
    for len in [0usize, 1, 255, 256, 257, 10_000, 1_000_000] {
        let q = "A".repeat(len);
        assert_eq!(w.lookup_ci("name", &q, Tick(0)), Ok(vec![]), "len {len}");
    }
    let q = "\u{0149}".repeat(1_000);
    assert_eq!(w.lookup_ci("name", &q, Tick(0)), Ok(vec![]));
}

// ---------------------------------------------------------------- errors

#[test]
fn errors_unknown_inverse_unindexed_and_non_string() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "name", s("Edwin"), t(0)).unwrap();
    assert_eq!(
        w.lookup_ci("nope", "x", Tick(0)),
        Err(Error::UnknownAttribute("nope".into()))
    );
    assert_eq!(
        w.lookup_ci("child_of", "x", Tick(0)),
        Err(Error::UnknownAttribute("child_of".into()))
    );
    assert_eq!(
        w.lookup_ci("bio", "x", Tick(0)),
        Err(Error::NotIndexed("bio".into()))
    );
    for (attr, expected) in [
        ("rank", ValueType::I64),
        ("alive", ValueType::Bool),
        ("owns", ValueType::Ref),
        ("parent_of", ValueType::Ref),
    ] {
        assert_eq!(
            w.lookup_ci(attr, "x", Tick(0)),
            Err(Error::WrongValueType {
                attr: attr.into(),
                expected,
                got: ValueType::Str
            }),
            "{attr}"
        );
    }
    // the view reports the same errors
    let v = w.at_canon(w.latest_tx());
    assert_eq!(
        v.lookup_ci("rank", "x", Tick(0)),
        Err(Error::WrongValueType {
            attr: "rank".into(),
            expected: ValueType::I64,
            got: ValueType::Str
        })
    );
    assert_eq!(
        v.lookup_ci("bio", "x", Tick(0)),
        Err(Error::NotIndexed("bio".into()))
    );
}

// ---------------------------------------------------------------- the folded index

#[test]
fn only_indexed_string_attributes_get_folded_rows_and_they_mirror_v() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "name", s("Edwin"), t(0)).unwrap();
    w.assert(e.edwin, "bio", s("Long Story"), t(0)).unwrap(); // unindexed
    w.assert(e.edwin, "rank", Value::I64(3), t(0)).unwrap();
    w.assert(e.edwin, "alive", Value::Bool(true), t(0)).unwrap();
    w.assert(e.edwin, "owns", Value::Ref(e.varrow), t(0))
        .unwrap();
    let entry = only(w.assert(e.greg, "name", s("GREG"), t(1)).unwrap());
    w.retract(e.edwin, "name", s("Edwin"), t(9)).unwrap();
    w.correct(entry, rep(Some(s("Gregor")), None, None))
        .unwrap();
    w.assert(e.mara, "title", s("Queen"), t(2)).unwrap();

    let d = w.index_dump();
    let str_attrs: std::collections::BTreeSet<AttrId> = d.v.iter().map(|r| r.0).collect();
    // V only has the indexed non-ref attributes; v_fold only its string ones
    let fold_attrs: std::collections::BTreeSet<AttrId> = d.v_fold.iter().map(|r| r.0).collect();
    assert!(!d.v_fold.is_empty());
    assert!(fold_attrs.is_subset(&str_attrs));
    assert!(d.v_fold.iter().all(|r| matches!(r.2, Value::Str(_))));
    // mirror: same rows (including DELETED markers) as V's string rows
    let mut expected: Vec<_> =
        d.v.iter()
            .filter_map(|(a, v, en, tk, tx, m)| match v {
                Value::Str(text) => Some((*a, fold_for_search(text), v.clone(), *en, *tk, *tx, *m)),
                _ => None,
            })
            .collect();
    // key order: attr, folded (as Str encoding), original encoding, entity, newest tick first, newest tx first
    expected.sort_by(|x, y| {
        (
            x.0,
            Value::Str(x.1.clone()).encode().unwrap(),
            x.2.encode().unwrap(),
            x.3,
            std::cmp::Reverse(x.4),
            std::cmp::Reverse(x.5),
        )
            .cmp(&(
                y.0,
                Value::Str(y.1.clone()).encode().unwrap(),
                y.2.encode().unwrap(),
                y.3,
                std::cmp::Reverse(y.4),
                std::cmp::Reverse(y.5),
            ))
    });
    assert_eq!(d.v_fold, expected);
    // bio (unindexed), rank, alive, owns have no folded rows
    assert!(
        d.v_fold
            .iter()
            .all(|r| !matches!(r.2, Value::I64(_) | Value::Bool(_) | Value::Ref(_)))
    );
    assert!(
        d.v_fold
            .iter()
            .all(|r| r.1 != fold_for_search("Long Story"))
    );
    clean(&w);
}

#[test]
fn a_world_with_no_string_facts_has_no_folded_rows() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "rank", Value::I64(3), t(0)).unwrap();
    w.assert(e.edwin, "owns", Value::Ref(e.varrow), t(0))
        .unwrap();
    assert!(w.index_dump().v_fold.is_empty());
}

#[test]
fn exact_lookup_stays_case_sensitive() {
    let (mut w, e) = mem();
    w.assert(e.edwin, "name", s("Edwin"), t(0)).unwrap();
    assert_eq!(w.lookup("name", &s("edwin"), Tick(0)).unwrap(), vec![]);
    assert_eq!(w.lookup("name", &s("EDWIN"), Tick(0)).unwrap(), vec![]);
    assert_eq!(
        w.lookup("name", &s("Edwin"), Tick(0)).unwrap(),
        vec![e.edwin]
    );
    assert_eq!(ci(&w, "name", "edwin", 0), vec![e.edwin]);
    // and decomposed vs precomposed are different exact values
    w.assert(e.greg, "name", s("\u{00E9}"), t(0)).unwrap();
    assert_eq!(w.lookup("name", &s("e\u{0301}"), Tick(0)).unwrap(), vec![]);
    assert_eq!(ci(&w, "name", "e\u{0301}", 0), vec![e.greg]);
}

// ---------------------------------------------------------------- doctor

const FOLD_INDEXES: [IndexName; 2] = [IndexName::VFold, IndexName::VFoldLatest];

fn world_for_doctor() -> World {
    let (mut w, e) = mem();
    w.assert(e.edwin, "name", s("Edwin"), t(1)).unwrap();
    w.assert(e.greg, "name", s("Greg"), t(2)).unwrap();
    w.assert(e.mara, "name", s("MARA"), t(3)).unwrap();
    w.retract(e.greg, "name", s("Greg"), t(9)).unwrap();
    w
}

fn mismatch_on(rep: &Report, which: IndexName) -> bool {
    rep.findings
        .iter()
        .any(|f| matches!(f, Finding::IndexMismatch { index, .. } if *index == which))
}

#[test]
fn doctor_is_clean_on_a_world_with_folded_rows() {
    let w = world_for_doctor();
    assert!(!w.index_dump().v_fold.is_empty());
    clean(&w);
}

#[test]
fn doctor_flags_a_removed_or_flipped_folded_row_for_that_index_only() {
    let all = [
        IndexName::E,
        IndexName::R,
        IndexName::ELatest,
        IndexName::RLatest,
        IndexName::V,
        IndexName::VLatest,
        IndexName::VFold,
        IndexName::VFoldLatest,
    ];
    for which in FOLD_INDEXES {
        for nth in [0usize, 1, 3] {
            for flip in [false, true] {
                let mut w = world_for_doctor();
                let tamper = if flip {
                    Tamper::FlipOp { index: which, nth }
                } else {
                    Tamper::RemoveRow { index: which, nth }
                };
                w.tamper_index(tamper);
                let rep = doctor(&w, &DoctorOptions::default());
                assert!(
                    mismatch_on(&rep, which),
                    "{which:?} #{nth} flip={flip}: {:#?}",
                    rep.findings
                );
                for other in all {
                    if other != which {
                        assert!(
                            !mismatch_on(&rep, other),
                            "{which:?} #{nth}: {other:?} reported: {:#?}",
                            rep.findings
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn doctor_flags_a_phantom_folded_row_and_decodes_garbage() {
    for which in FOLD_INDEXES {
        let mut w = world_for_doctor();
        w.tamper_index(Tamper::InsertRawKey {
            index: which,
            key: b"F".to_vec(),
        });
        let rep = doctor(&w, &DoctorOptions::default());
        assert!(
            rep.findings
                .iter()
                .any(|f| matches!(f, Finding::IndexDecode { index, .. } if *index == which)),
            "{which:?}: {:#?}",
            rep.findings
        );
        // reads keep working
        let _ = w.lookup_ci("name", "edwin", Tick(5));
        let _ = w.index_dump();
    }
}

#[test]
fn tampering_the_folded_index_changes_the_answer_but_not_the_exact_one() {
    let mut w = world_for_doctor();
    let e = w.entity_by_handle("edwin").unwrap();
    assert_eq!(ci(&w, "name", "EDWIN", 5), vec![e]);
    // remove every folded row of the main index (all of them are for `name`)
    let n = w.index_dump().v_fold.len();
    for _ in 0..n {
        w.tamper_index(Tamper::RemoveRow {
            index: IndexName::VFold,
            nth: 0,
        });
    }
    assert_eq!(
        w.at_canon_main(w.latest_tx())
            .lookup_ci("name", "EDWIN", Tick(5))
            .unwrap(),
        vec![]
    );
    assert_eq!(w.lookup("name", &s("Edwin"), Tick(5)).unwrap(), vec![e]);
    assert!(mismatch_on(
        &doctor(&w, &DoctorOptions::default()),
        IndexName::VFold
    ));
}

// ---------------------------------------------------------------- files

#[test]
fn reopening_a_file_world_gives_the_same_lookup_ci_answers_and_a_clean_doctor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let questions = |w: &World| {
        let mut out = Vec::new();
        for at in [-1, 0, 1, 5, 10, 11, 100] {
            for q in [
                "edwin",
                "EDWIN",
                "grey king",
                "Stra\u{00DF}e",
                "STRASSE",
                "\u{00E9}mile",
                "x",
            ] {
                out.push(w.lookup_ci("name", q, Tick(at)).unwrap());
            }
        }
        out
    };
    let (before, dump_before, tx) = {
        let mut w = World::create(&path, schema()).unwrap();
        let e = populate(&mut w);
        let n = only(w.assert(e.edwin, "name", s("Edwin"), t(0)).unwrap());
        w.assert(e.edwin, "name", s("EDWIN"), t(3)).unwrap();
        w.assert(e.greg, "name", s("Stra\u{00DF}e"), t(5)).unwrap();
        w.assert(e.mara, "name", s("E\u{0301}mile"), t(5)).unwrap();
        w.retract(e.mara, "name", s("E\u{0301}mile"), t(10))
            .unwrap();
        w.correct(n, rep(Some(s("Grey King")), t(1), None)).unwrap();
        let q = questions(&w);
        assert!(q.iter().any(|x| !x.is_empty()));
        (q, w.index_dump(), w.latest_tx())
    };
    let w = World::open(&path).unwrap();
    assert_eq!(*w.recovery(), RecoveryReport::default());
    assert_eq!(w.latest_tx(), tx);
    assert_eq!(questions(&w), before);
    assert_eq!(w.index_dump().v_fold, dump_before.v_fold);
    let rep = doctor(&w, &DoctorOptions::default());
    assert!(rep.is_clean(), "{:#?}", rep.findings);
    let rep = doctor_log(&path, &DoctorOptions::default()).unwrap();
    assert!(rep.is_clean(), "{:#?}", rep.findings);
}
