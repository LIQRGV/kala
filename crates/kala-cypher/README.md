# kala-cypher

A bounded, hand-written Cypher parser and compiler to `kala-query`'s shared
IR. It depends only on `kala-core` and `kala-query`. The executor and store are
shared with Datalog. `prepare(world, text)` returns a `Prepared` plan and parameter
kinds; `parse` and `prepare_parsed` allow callers to inspect attributes first.

## Supported syntax

```cypher
MATCH (e {kala_handle: 'edwin'})-[:ally_of]->(a)
AT TICK -210 CANON 10
WHERE a.name IS NOT NULL
RETURN DISTINCT a, a.name AS name
ORDER BY name ASC SKIP 0 LIMIT 10;
```

One or more MATCH clauses support comma-separated paths, anonymous nodes,
multiple node labels, property maps, chains, typed relationships in either or
both directions, relationship variables and relationship property maps.
WHERE supports `=`, `<>`, `<`, `<=`, `>`, `>=`, `AND`, `OR`, `XOR`, `NOT`,
parentheses, `IS NULL` and `IS NOT NULL`. As in openCypher, `IS [NOT] NULL`
binds tighter than comparisons (`a = b IS NULL` is `a = (b IS NULL)`), and a
comparison chain `a < b <= c` means `a < b AND b <= c`. RETURN items are
variables, properties, literals or parameters, with optional aliases and
DISTINCT; two columns with the same name (`RETURN a, a`) are a syntax error.
ORDER BY supports
several keys and ASC/DESC; SKIP/LIMIT accept non-negative integers or parameters.
With DISTINCT, sort keys must be returned expressions or aliases.

Keywords ignore case; identifiers and labels match exact case. Backtick names,
Cypher string escapes (including Unicode), `//` and `/* */` comments, and one
trailing semicolon are accepted. Queries are at most 64 KiB, expression nesting
at most 32, and pattern/expression elements at most 256. Errors include a line
and column. UNION, dynamic property access, arbitrary functions and
unbounded traversal are not supported yet.

## Pipelines, aggregates and paths

`OPTIONAL MATCH pattern [AT TICK t] [WHERE predicate]` is a left outer match.
Its WHERE filters the optional match; if all candidates fail, the original row
survives with NULL for newly introduced names. OPTIONAL MATCH may start a query.
Property access and path functions on NULL return NULL.

`WITH [DISTINCT] item [AS alias], … [ORDER BY …] [SKIP n] [LIMIT n] [WHERE …]`
projects a new scope. Only projected names survive; `WITH *` keeps all names.
Non-variable expressions need aliases. Carried graph variables keep their
binding tick; projected property values are not reread. WHERE sees the projected
names. `UNWIND expr AS x` expands parameter, literal or variable lists in order,
keeping duplicate elements; NULL and [] produce no rows, other scalars error.
Lists can be nested to depth 32. Both retained memory and a list's expanded
materialized size must fit the query byte limit, including lists built by WITH.

RETURN and WITH support `count(*)`, `count(x)`, `count(DISTINCT x)`,
`collect(x)`, `collect(DISTINCT x)`, `min`, `max`, `sum`, and `avg`.
Non-aggregate items form implicit grouping keys. NULL operands are skipped;
count(*) counts rows including NULL rows. Collect keeps duplicates and input
row order (use WITH ORDER BY to select that order). Min/max require one value
type; sum checks i64 overflow or uses f64; avg always returns f64.
Without grouping keys, empty input produces one row: count=0, collect=[],
sum=0, min/max/avg=NULL. With keys it produces no rows. Datalog's set aggregate
and empty-input behavior are unchanged. Nested aggregates and aggregates in
WHERE are errors.

A relationship may specify `*min..max`, `*..max` (minimum 1), or `*n`.
An upper bound is mandatory and at most **10** (`MAX_VAR_LENGTH` in kala-query).
`*0..n` includes the start node and an empty relationship list. Every hop seeks
from its frontier node and is covered by step, row, memory and cancellation
limits; exceeding a limit fails, never truncates the answer.

`MATCH p = (a)-[rs:ally_of*1..3]-(b) RETURN p` binds a Path and the ordered
relationship list `rs`. Fixed multi-edge and zero-edge paths are also supported:
`p = (a)-[:x]->(b)<-[:y]-(c)` and `p = (a)`. `length(p)`, `nodes(p)`,
`relationships(p)`, and `size(list)` are the supported list/path functions.
A path never repeats an edge; all relationship patterns in one MATCH also
exclude edges used by its other patterns. Nodes may repeat; a bidirectional
relationship is still one edge. Separate MATCH clauses may reuse an edge.
Path equality and ordering compare node sequences, then edge sequences, then
stored-direction flags, lexicographically. Properties in Bolt Path values are
rendered at the plan's default tick, just like Node/Relationship values.

## Catalogue and server metadata

`CALL db.labels()`, `CALL db.relationshipTypes()` and `CALL db.propertyKeys()`
work in the CLI and over Bolt. They return sorted schema names at the pinned
canon; labels also include distinct active `label` values at the query tick,
and property keys include `kala_handle`. Relationship types include inverse
names and deprecated readable attributes. A catalogue CALL may carry
`AT TICK t CANON c`. `YIELD col [AS alias], … [WHERE …]` flows through the
ordinary WITH/RETURN pipeline. Arguments and YIELD * are unsupported.

`CALL dbms.components()` and `SHOW DATABASES`, `SHOW DATABASE name`,
`SHOW DEFAULT DATABASE`, `SHOW HOME DATABASE` require Bolt and likewise support
YIELD/WHERE and projection. The adapter supplies a language-neutral metadata
table; only a pure metadata source followed by WITH/RETURN is allowed. These
statements cannot carry graph reads under their metadata authorization.
See [driver metadata answers](../kala-bolt/README.md#neo4j-drivers-and-tools).

## Time and graph mapping

The first MATCH or catalogue CALL's `AT TICK` sets the default; without it the default is the
latest tick at the pinned canon. Later MATCH clauses can read at another tick;
clauses without AT TICK use the default. `CANON` may occur once, after AT TICK.
Both accept integer literals or `$parameters`. A property's read tick is the
clause where its variable was first bound. Nodes exist from their definition
at the canon; labels and properties are read at the clause tick.

Node type names are labels. Other labels match active values of `label`; all
labels in a pattern must hold. An unconstrained node pattern scans all node
types, excluding edges. Relationship types name ref attributes, including
inverse names. Bidirectional attributes match both ways regardless of the arrow;
an undirected match does not duplicate a direction. A self-edge matches once.
One MATCH cannot use the same edge twice; separate MATCH clauses may reuse it.
Likewise a relationship variable names one relationship per MATCH (using `r`
twice in one clause is a syntax error); a later MATCH reusing `r` constrains it
to the same edge.

`kala_handle` is the permanent, read-only world handle, looked up through the
handle index for a string literal or a `$parameter`; `handle` is ordinary
data. Unknown labels/types match nothing; unknown properties yield NULL,
including names not yet defined at the requested canon. Name resolution uses
the latest name table, as elsewhere in Kala.

## Results and NULL

| Behavior | Cypher | Datalog |
| --- | --- | --- |
| Missing property | NULL | No binding; no row |
| Comparison with NULL | NULL | No NULL datum |
| Ordering incomparable types (`'a' < 5`, `n < 1`) | NULL | Type error |
| Boolean predicates | Three-valued; WHERE retains only true | Boolean; negation uses `not` |
| NULL in output | One NULL cell; row retained | Not produced |
| Duplicates | Bag unless RETURN DISTINCT | Set |
| Unmatched OPTIONAL MATCH | Retain left row, new bindings NULL | Explicit set negation |
| Aggregates on NULL | Skip NULL; count(*) still counts the row | No NULL |
| Zero rows without grouping keys | One aggregate row | No aggregate row |
| UNWIND NULL or [] | No rows | Not applicable |

`<`, `<=`, `>` and `>=` compare only mutually comparable values: two numbers
(integers and floats compare numerically), two strings, two booleans, or two
lists of such values (element-wise, then by length). Any other pair, including
a node or relationship on either side, gives NULL, so `WHERE n.name < 5` and
`WHERE NOT (n.name < 5)` both drop the row. `=` and `<>` across different
types are false and true, as in Neo4j.

ORDER BY is stable, with NULL last ascending and first descending. Supported
mixed types sort as nodes, relationships, lists, strings, booleans, numbers,
paths, NULL; strings use UTF-8 byte order and integers/floats compare numerically.
Without ORDER BY, execution order is deterministic. Projection/distinctness
precede ordering and pagination. Budgets error rather than silently truncate.

## Known differences from Neo4j

- One declared type per attribute; values cannot mix stored scalar types.
- Stored lists represent sets: cardinality-many properties are sorted lists, without
  duplicates. A missing property is NULL, including a missing many property.
- A cardinality-one property with overlapping values is also a sorted list,
  preserving all asserted values and the store's cardinality warning.
- One immutable node type plus active `label` values supplies labels.
- A ref attribute read as a property returns a node/relationship reference,
  or a list for many, rather than requiring a relationship pattern.
- Bolt Node, Relationship and Path objects are enriched at the plan's **default**
  tick even when bound in a later clause with a different tick. Explicit
  `RETURN v.key` reads at the variable's binding clause tick.
- Native ref attributes can target edge entities. Both named and anonymous
  relationship traversals preserve those references, so an unlabelled endpoint
  may bind an edge; a standalone all-node scan still excludes edges. This keeps
  the same fact-expansion semantics and exact Plan parity as Datalog.

Parameters accept scalars, NULL and lists. Property lists are sorted sets;
supplied parameter lists retain their input order for equality. Comparisons
with NULL inside lists use three-valued equality. The embedded `QueryParams`
API supports these values; the existing scalar `Params` alias remains valid.
Numeric parameters: over Bolt, a parameter compared with an `f64` attribute
accepts a PackStream Integer when it converts exactly (|n| <= 2^53); a larger
Integer is a type error. On the CLI, an untyped `--param` is an integer when
it parses as one, and a float only when it looks numeric (`1.5`, `.5`, `2e3`),
so `nan`, `inf` and `Infinity` stay strings.

The CLI uses `kala query WORLD --lang cypher QUERY` and
`kala explain WORLD --lang cypher QUERY`; its default remains Datalog.
Unprefixed Bolt RUN text selects Cypher, as does `// kala:cypher` on the first
line. `// kala:datalog` selects Datalog. On the system world, all explicit
`password_hash` reads are forbidden before evaluation, including comparisons,
property maps and sort keys. Returning account nodes redacts that property.

The upper bound of ten hops, collect's preserved input order, explicit path
sequence ordering, and enrichment at the default tick are documented differences
from Neo4j. Secret refusal covers every new expression position, including
aggregates, WITH, UNWIND, optional maps and variable-length relationship maps;
account nodes inside Paths use the same property redaction as Node results.

## Writing a world

A statement can start with a reading pipeline. Kala evaluates the whole
pipeline on one snapshot, then applies the write clauses once per row in
order. A statement starting with a write has one input row. Each statement
allows 10,000 input rows and 10,000 write primitives. The row limit counts
the rows that reach the write clauses, not the rows a MATCH scans on the
way. The reads that SET, REMOVE and DELETE make are bounded by 256 index
keys per primitive (2,560,000 a statement) and by the statement's step
budget and deadline; one SET takes at most 256 values for a property. Each
limit has its own error message. A failure rolls back
the transaction. For bulk imports, pass a list of scalar handles to
`UNWIND $handles AS h MERGE (n:CHARACTER {kala_handle:h})`.
For parallel lists, pass the row positions as `$indices` and index each list
with the same position.

```cypher
UNWIND $indices AS i
MERGE (n:CHARACTER {kala_handle:$handles[i]})
ON CREATE SET n.name = $names[i]
```

Positions start at zero. Negative offsets count from the end, and a missing
element produces NULL. `[…]` indexes a list parameter, a list variable or a
row field. A graph property is never indexed (`n.alias[0]` is a syntax
error at the `[`): an attribute's values are an unordered set. Drivers can
also supply a list of maps directly:

```cypher
UNWIND $rows AS row
MERGE (n:NPC {kala_handle: row.handle})
SET n.name = row.name AT TICK row.tick
```

For example, pass `rows=[{"handle":"kael","name":"Kael","tick":-20}]`.
Read a field with `row.key` in a read or write expression. A missing field
yields NULL. A field holds a scalar, NULL or a list of scalars. Its type must
agree across rows that supply a non-NULL value. Keys are data, including
keys that happen to share a graph property's name. Maps must arrive through
a direct parameter UNWIND. Returning, comparing or storing the row itself,
nesting maps and `SET n += row` are errors. `UNWIND … CREATE` requires both
`read` and `write` over Bolt.

Parameters take the kind their position gives them, as in a read query: a
handle string given where the reading part compares a ref attribute
(`MATCH (c {located_in:$place})`) names that entity, and a value of the
wrong type is a type error before anything runs. A string written to a ref
attribute (`SET c.located_in = $place`, `SET c.located_in = 'varrow'`, a
CREATE property map) is a handle too, as in a Datalog form; a handle nobody
holds is the `unknown entity` error.

A row field behaves the same. Where the reading part compares it with a ref
attribute (`MATCH (c {located_in: row.place})`, `WHERE c.located_in =
row.place`), its string is a handle or a formatted ID and names that entity;
a handle nobody holds is the same type error a scalar parameter gives, never
an empty result. One field cannot be both: a field compared with a ref
attribute and also used as plain data (compared with a string property,
written as a `kala_handle` or to a non-ref attribute) is a type error that
names the field. Pass the handle in two fields.

The following pairs show the primitive operations for one input row.
The SET, REMOVE and DELETE examples assume a preceding MATCH binds the named
variables. Here `g` is Greg, `a` is Ashford and `r` is their `located_in` edge.

| Cypher | Datalog equivalent |
| --- | --- |
| `CREATE (:CHARACTER:Scout {kala_handle:'s', name:'S'}) AT TICK 700` | `[:at 700 [:create :CHARACTER :s] [:assert :s :label "Scout"] [:assert :s :name "S"]]` |
| `CREATE (g)-[:located_in {since:3}]->(a) AT TICK 700` | `[:at 700 [:assert :greg :located_in :ashford] [:assert [:greg :located_in :ashford] :since 3]]` |
| `SET g.located_in = a AT TICK 225` | `[:set :greg :located_in :ashford :at 225]` |
| `SET g += {alias:['Grey','Greg']}` | `[:set :greg :alias ["Grey" "Greg"]]` |
| `SET g:Scout` / `REMOVE g:Scout` | `[:assert :greg :label "Scout"]` / `[:retract :greg :label "Scout"]` |
| `REMOVE g.alias` or `SET g.alias = NULL` | `[:set :greg :alias []]` |
| `DELETE r AT TICK 226` | `[:retract :greg :located_in :ashford :at 226]` |
| `DELETE n` | One `[:retract …]` per held state. The node must have no active relationships. |
| `DETACH DELETE n` | The same state retracts plus one `[:retract …]` per active state relationship in either direction. |
| `MERGE (:CHARACTER {kala_handle:'s'})` | `[:create :CHARACTER :s :if-missing]` |
| `MERGE (g)-[:located_in]->(a)` | `[:assert :greg :located_in :ashford]` |

`REMOVE n.x`, `SET n.x = NULL` and `REMOVE n:Label` on a property (or on
`label`) that nobody declared change nothing; the Datalog form
`[:set … :x []]` on an unknown attribute is an error. SET and REMOVE on a
variable that is NULL (an OPTIONAL MATCH that found nothing) skip that row,
as DELETE does.

The first label gives a new node its permanent type. A `label` attribute
declared by inference is cardinality-many (in Datalog too; a world written
by an older Kala replays unchanged). Additional labels are
string values of `label`. Lists are sets of property values. A node variable
on the right of SET supplies a reference. Replacing a property writes the
difference between its old and new sets, including the explicit retracts.
Handles are permanent. Use `SET n += {key: value}` to update selected keys.
Kala refuses whole-map replacement with `SET n = {…}`.

A node MERGE uses exactly one type and its permanent `kala_handle`. A
relationship MERGE requires two bound endpoints and a type. Use
`ON CREATE SET` for a new node or newly asserted relationship and
`ON MATCH SET` for an existing match. CREATE always creates its nodes.
Relationship writes require an arrow. Bidirectional attributes have one
stored edge, and inverse names select the stored edge with reversed endpoints,
whether written as a relationship (`CREATE (c)-[:child_of]->(p)`), in SET
(`SET c.child_of = p`) or in a CREATE property map (`{child_of: p}`).

DELETE preserves node identity, handle, history and events. Events never
block a plain DELETE and are never retracted, by DELETE or DETACH DELETE.
The values of deprecated attributes (including the `name@old` a
`[:migrate]` leaves behind) are left as they are: a deprecated attribute
accepts no retract, so its values neither end nor count as relationships. A detached
relationship's own properties keep their history too. `kala doctor` can
warn about a property held on an inactive edge. Summaries count retracted
relationships and omit a nodes-deleted counter.

A read-side `AT TICK` takes one integer literal or parameter. After the
write clauses, `AT TICK` takes one of: an integer literal, a parameter, a
variable bound by `UNWIND` or `WITH`, a row field (`row.tick`), an indexed
list parameter (`$ticks[i]`), or NULL. Anything else (an aggregate, a
function call, a graph property such as `n.age`) is a syntax error at the
tick. Each integer selects that row's write tick.
NULL uses the read-side tick if supplied, then the transaction default,
then the transaction's starting latest tick. A trailing integer literal or
scalar tick parameter also supplies the MATCH tick when MATCH has none.

Bolt's write summary reports `kala.tick_min`, `kala.tick_max` and
`kala.tick_count` for the reported operations. Each operation counts once,
including a no-op. A same-tick conflict names the subject, the attribute,
the tick and what holds it: the occupying entry with its `[:correct …]`
form, or the earlier staged operation. In a map-row write or a write with a
per-row tick the same message starts with the zero-based input row
(`row 3: …`).
Corrections use the Datalog `[:correct …]` form.

### Declaring a property type

```cypher
CREATE CONSTRAINT [name] [IF NOT EXISTS] FOR (n:Label) REQUIRE n.prop IS :: INTEGER
```

declares attribute `prop` exactly as `[:attribute :prop :i64]` does. The
types are `INTEGER`/`INT` (i64), `FLOAT` (f64), `STRING` and
`BOOLEAN`/`BOOL`; `IS TYPED TYPE` is accepted too. It is a statement of its
own, needs only `write` over Bolt and its summary type is `s`.

A constraint only ever adds a declaration. With `IF NOT EXISTS`, a `prop`
that is already an attribute (whatever its type or other labels) makes the
statement a no-op: nothing is declared, not even a new label. Without it,
the same type is a no-op and a different type is refused, for an
administrator too (`attribute :prop already exists with type T; change it
with // kala:datalog [:attribute …] or [:migrate …]`): an existing
definition is changed with the Datalog forms.

Differences from Neo4j: Kala attributes are not per label, so the
declaration holds for every node type; the label must be a node type, or
becomes one as schema-on-write declares it, **permanently** (a node type
cannot be removed), so mind its spelling; the constraint name is accepted
and ignored. Uniqueness, existence, key and relationship constraints,
`CREATE INDEX` and `DROP …` are refused.

A write without RETURN produces zero records. RETURN accepts bound variables,
property access, literals and parameters. Auto-commit renders graph values
at each row's write tick with final IDs and committed properties. Within BEGIN,
write RETURN can read staged properties. Sending a provisional identity
protects it against renumbering at COMMIT. A later MATCH still reads the
transaction snapshot. Ordering, aggregation and a new reading pipeline
after a write are outside this subset.
