# Datalog write parameters

A write form accepts `?name` wherever it takes a handle, a value, an entry number or a tick. Every parameter in the form must be supplied. Write forms discover parameters from their positions and omit the query-only `:in` clause.

```clojure
[[:create :NPC ?who :if-missing]
 [:assert ?who :name ?display :at ?tick]]
```

Supply `who` as a handle string, `display` as the name to store and `tick` as a signed 64-bit integer. A reference may also use a formatted ID such as `NPC#1`. Resolution sees handles created earlier in the transaction. Strings preserve their quotes, brackets and newlines.

Over Bolt, send the form as a Datalog statement and the values in the driver's parameter map. For a local file, pass `--param who=rook --param display=Rook --param tick=-17` to `kala transact`. In the REPL, use `.param` as for queries.

Parameters are scalars. An existing attribute supplies its value type: a string given for a `ref` attribute is a handle (or a formatted ID), for a `string` attribute it is text. "Existing" means defined in the world, declared or first written by an earlier operation of the same form, or staged by an earlier form of the same transaction: each form is bound just before it is staged, against the schema its transaction sees. In one auto-commit RUN or `kala transact` file, each form is its own transaction and sees the forms committed before it.

A new attribute takes the supplied value's type under the world's schema-on-write rules when the value is an integer, a float or a bool. A **string** parameter written to an attribute that is not defined is refused before anything is staged: a string cannot say whether it is a handle (`:edwin`, a relationship) or text (`"edwin"`, a string property), and a wrong guess would store every relationship of an import as text. The error names the parameter and the attribute; declare the attribute first, in the same form or an earlier one:

```clojure
[[:attribute :friend_of :ref :many]
 [:assert ?who :friend_of ?friend :at ?tick]]
```
 Each member of a `:set` vector may be a separate parameter. A correction's entry parameter takes a nonnegative integer. Verbs, attribute names, node types, precision, operation tags and schema forms must be literal. `kala dump` emits literal forms.
