//! Shared write inputs so CLI and Bolt exercise the same validation paths.
pub const WRITES: &[(&str, &str, &str)] = &[
    ("datalog", "a.b", "[:attribute :a.b :string]"),
    (
        "datalog",
        "Carries.By",
        "[:attribute :carries :ref :inverse :Carries.By]",
    ),
    ("datalog", "N.PC", "[:node-type :N.PC]"),
    ("datalog", "npc.durn", "[:create :NPC :npc.durn]"),
    ("datalog", "a.b", "[:assert :edwin :a.b 1]"),
    ("cypher", "a.b", "CREATE (:NPC {`a.b`: 1})"),
    ("cypher", "N.PC", "CREATE (:`N.PC`)"),
    ("cypher", "is.at", "CREATE (:NPC)-[:`is.at`]->(:PLACE)"),
    (
        "cypher",
        "npc.durn",
        "CREATE (:NPC {kala_handle:'npc.durn'})",
    ),
    (
        "cypher",
        "npc.durn",
        "MERGE (n:NPC {kala_handle:'npc.durn'})",
    ),
];

pub const DATALOG_READ: &str = "[:find ?e :where [?e :a.b ?v]]";
pub const CYPHER_READ: &str = "MATCH (n {kala_handle:'edwin'}) RETURN n.`a.b` AS value";
