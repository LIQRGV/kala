//! Bolt adapters classify through kala-lang and authorize parsed write operations.
use kala_lang::write::{parse_write, requires_read};
use kala_lang::{Cypher, Datalog, Statement, classify, refuse_nothing};

#[test]
fn classifies_datalog_write_forms_and_queries_without_a_world() {
    for text in [
        "[:assert :greg :alias \"Ned\" :at 300]",
        "[:assert :greg :alias \"Ned\"]\n[:retract :greg :alias \"Old\"]",
        "[:at -300 [:concurrency :weak] [:create :CHARACTER :new]]",
        "; comment\n[[:node-type :CHARACTER] [:attribute :age :i64]]",
    ] {
        assert!(
            matches!(
                classify(&Datalog, text, &refuse_nothing).unwrap(),
                Statement::Writes
            ),
            "{text}"
        );
    }
    assert!(matches!(
        classify(&Datalog, "[:find ?e :where [?e :name ?n]]", &refuse_nothing).unwrap(),
        Statement::Query
    ));
}

#[test]
fn classifies_server_metadata_and_cypher_queries_separately() {
    for text in ["SHOW DATABASES", "CALL dbms.components()"] {
        assert!(
            matches!(
                classify(&Cypher, text, &refuse_nothing).unwrap(),
                Statement::Server(_)
            ),
            "{text}"
        );
    }
    assert!(matches!(
        classify(&Cypher, "MATCH (n) RETURN n", &refuse_nothing).unwrap(),
        Statement::Query
    ));
}

#[test]
fn a_write_form_needs_read_if_any_operation_reads_existing_data() {
    for text in [
        "[:set :greg :name \"Ned\"]",
        "[:create :CHARACTER :greg :if-missing]",
        "[:correct 67 :delete]",
        "[:correct 67 :replace :at -280]",
        "[:migrate :age :string]",
        "[[:assert :greg :alias \"Ned\"] [:set :greg :name \"Ned\"]]",
        "[:at -10 [:concurrency :weak] [:create :CHARACTER :greg :if-missing]]",
    ] {
        assert!(requires_read(&parse_write(text).unwrap()), "{text}");
    }
    for text in [
        "[:assert :greg :alias \"Ned\"]",
        "[:retract :greg :alias \"Ned\"]",
        "[:create :CHARACTER :greg]",
        "[:attribute :age :i64]",
        "[:node-type :CHARACTER]",
        "[[:attribute :alias :string :many] [:assert :greg :alias \"Ned\"]]",
    ] {
        assert!(!requires_read(&parse_write(text).unwrap()), "{text}");
    }
}
