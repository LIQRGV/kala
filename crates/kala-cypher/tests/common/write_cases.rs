//! Write cases shared by the byte-for-byte and CLI dump oracles.
pub const CASES: &[(&str, &str)] = &[
    (
        "CREATE (:CHARACTER:Scout {kala_handle:'new_scout', alias:['One','Two'], name:null}) AT TICK -701",
        r#"[:at -701 [:create :CHARACTER :new_scout] [:assert :new_scout :label "Scout"] [:assert :new_scout :alias "One"] [:assert :new_scout :alias "Two"]]"#,
    ),
    (
        "MATCH (g {kala_handle:'greg'}), (a {kala_handle:'ashford'}) SET g.located_in = a AT TICK 225",
        "[:set :greg :located_in :ashford :at 225]",
    ),
    (
        "MATCH (g {kala_handle:'greg'}) SET g.name = 'Captain' AT TICK 700",
        r#"[:set :greg :name "Captain" :at 700]"#,
    ),
    (
        "MATCH (o {kala_handle:'osric'}) SET o.alias = ['One','Two'] AT TICK 0",
        r#"[:set :osric :alias ["One" "Two"] :at 0]"#,
    ),
    (
        "MATCH (g {kala_handle:'greg'}) SET g.name = NULL AT TICK 700",
        r#"[:retract :greg :name "Greg" :at 700]"#,
    ),
    (
        "MATCH (g {kala_handle:'greg'}) REMOVE g.name AT TICK 700",
        r#"[:retract :greg :name "Greg" :at 700]"#,
    ),
    (
        "MATCH (g {kala_handle:'greg'}) SET g:Guide AT TICK 700",
        r#"[:assert :greg :label "Guide" :at 700]"#,
    ),
    (
        "MATCH (g {kala_handle:'greg'}) REMOVE g:Soldier AT TICK 700",
        r#"[:retract :greg :label "Soldier" :at 700]"#,
    ),
    (
        "MATCH (g {kala_handle:'greg'}), (p {kala_handle:'varrow'}) CREATE (g)-[r:present {disguised:true}]->(p) AT TICK 700",
        "[:at 700 [:assert :greg :present :varrow] [:assert [:greg :present :varrow] :disguised true]]",
    ),
    (
        "MATCH (g {kala_handle:'greg'}), (m {kala_handle:'mara'}) CREATE (m)<-[:ally_of]-(g) AT TICK 700",
        "[:assert :mara :ally_of :greg :at 700]",
    ),
    (
        "MATCH (m {kala_handle:'mara'})-[r:present]->(p {kala_handle:'graymoor'}) SET r.disguised = true AT TICK 700",
        "[:set [:mara :present :graymoor] :disguised true :at 700]",
    ),
    (
        "MATCH (m {kala_handle:'mara'})-[r:present]->(p {kala_handle:'graymoor'}) DELETE r AT TICK 700",
        "[:retract :mara :present :graymoor :at 700]",
    ),
    (
        "MERGE (n:CHARACTER {kala_handle:'new_scout'}) ON CREATE SET n.name = 'Scout' AT TICK 700",
        r#"[:at 700 [:create :CHARACTER :new_scout :if-missing] [:set :new_scout :name "Scout"]]"#,
    ),
    (
        "MERGE (n:CHARACTER {kala_handle:'greg'}) ON MATCH SET n.alias = 'Old Greg' AT TICK 700",
        r#"[:at 700 [:create :CHARACTER :greg :if-missing] [:set :greg :alias "Old Greg"]]"#,
    ),
    (
        "MATCH (g {kala_handle:'greg'}), (p {kala_handle:'varrow'}) MERGE (g)-[:present]->(p) AT TICK 700",
        "[:assert :greg :present :varrow :at 700]",
    ),
    (
        "MATCH (k {kala_handle:'kael'}) DETACH DELETE k AT TICK 700",
        r#"[:at 700 [:retract :kael :name "Edwin"] [:retract :kael :located_in :ashford]]"#,
    ),
    (
        "MATCH (k {kala_handle:'greg'}) SET k += {alias:'Captain',name:null} AT TICK 700",
        r#"[:at 700 [:set :greg :alias "Captain"] [:set :greg :name []]]"#,
    ),
];
