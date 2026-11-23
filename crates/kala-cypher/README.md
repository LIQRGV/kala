# kala-cypher

A bounded, hand-written Cypher read parser and compiler to `kala-query`'s shared
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
parentheses, `IS NULL` and `IS NOT NULL`. RETURN items are variables, properties,
literals or parameters, with optional aliases and DISTINCT. ORDER BY supports
several keys and ASC/DESC; SKIP/LIMIT accept non-negative integers or parameters.
With DISTINCT, sort keys must be returned expressions or aliases.

Keywords ignore case; identifiers and labels match exact case. Backtick names,
Cypher string escapes (including Unicode), `//` and `/* */` comments, and one
trailing semicolon are accepted. Queries are at most 64 KiB, expression nesting
at most 32, and pattern/expression elements at most 256. Errors include a line
and column. OPTIONAL MATCH, WITH, functions/aggregates, variable-length paths,
UNION, CALL, dynamic property access and writes are not supported yet.

## Time and graph mapping

The first MATCH's `AT TICK` sets the default; without it the default is the
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

`kala_handle` is the permanent, read-only world handle; `handle` is ordinary
data. Unknown labels/types match nothing; unknown properties yield NULL,
including names not yet defined at the requested canon. Name resolution uses
the latest name table, as elsewhere in Kala.

## Results and NULL

| Behavior | Cypher | Datalog |
| --- | --- | --- |
| Missing property | NULL | No binding; no row |
| Comparison with NULL | NULL | No NULL datum |
| Boolean predicates | Three-valued; WHERE retains only true | Boolean; negation uses `not` |
| NULL in output | One NULL cell; row retained | Not produced |
| Duplicates | Bag unless RETURN DISTINCT | Set |

ORDER BY is stable, with NULL last ascending and first descending. Supported
mixed types sort as nodes, relationships, lists, strings, booleans, numbers,
NULL; strings use UTF-8 byte order and integers/floats compare numerically.
Without ORDER BY, execution order is deterministic. Projection/distinctness
precede ordering and pagination. Budgets error rather than silently truncate.

## Known differences from Neo4j

- One declared type per attribute; values cannot mix stored scalar types.
- Lists represent sets: cardinality-many properties are sorted lists, without
  duplicates. A missing property is NULL, including a missing many property.
- A cardinality-one property with overlapping values is also a sorted list,
  preserving all asserted values and the store's cardinality warning.
- One immutable node type plus active `label` values supplies labels.
- A ref attribute read as a property returns a node/relationship reference,
  or a list for many, rather than requiring a relationship pattern.
- Bolt Node and Relationship objects are enriched at the plan's **default**
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

The CLI uses `kala query WORLD --lang cypher QUERY` and
`kala explain WORLD --lang cypher QUERY`; its default remains Datalog.
Unprefixed Bolt RUN text selects Cypher, as does `// kala:cypher` on the first
line. `// kala:datalog` selects Datalog. On the system world, all explicit
`password_hash` reads are forbidden before evaluation, including comparisons,
property maps and sort keys. Returning account nodes redacts that property.
