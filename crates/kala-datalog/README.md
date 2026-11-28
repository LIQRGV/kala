# Datalog write parameters

A write form accepts `?name` wherever it takes a handle, a value, an entry number or a tick. Every parameter in the form must be supplied. Write forms discover parameters from their positions and omit the query-only `:in` clause.

```clojure
[[:create :NPC ?who :if-missing]
 [:assert ?who :name ?display :at ?tick]]
```

Supply `who` as a handle string, `display` as the name to store and `tick` as a signed 64-bit integer. A reference may also use a formatted ID such as `NPC#1`. Resolution sees handles created earlier in the transaction. Strings preserve their quotes, brackets and newlines.

Over Bolt, send the form as a Datalog statement and the values in the driver's parameter map. For a local file, pass `--param who=rook --param display=Rook --param tick=-17` to `kala transact`. In the REPL, use `.param` as for queries.

Parameters are scalars. An existing attribute supplies its value type. A new attribute takes the supplied value's type under the world's schema-on-write rules. Each member of a `:set` vector may be a separate parameter. A correction's entry parameter takes a nonnegative integer. Verbs, attribute names, node types, precision, operation tags and schema forms must be literal. `kala dump` emits literal forms.
