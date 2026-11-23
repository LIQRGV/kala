# Writing a story world with Kala

Kala keeps both story time (ticks) and the history of your edits (canon
transactions). States last until you explicitly retract them. A correction changes
what canon says about an earlier entry, while keeping the older canon readable.

Use a built `kala` binary on your PATH. Start from the repository root with the
sample schema and world:

```sh
kala load crates/kala-core/tests/fixtures/varrow.yaml varrow.kala
```

The sample has 50 entities, 309 fact entries, latest canon tx#12 and latest tick
700. To start your own world, load a fixture containing just `schema` (node types
and attributes), optionally `entities`. Schema changes still come from fixtures;
the terminal writes below create entities and edit facts in that schema.

## Write a scene

Save this as `scene.edn`:

```clojure
[:at 800
 [:create :CHARACTER :nerys]
 [:assert :nerys :name "Nerys"]
 [:assert :nerys :located_in :varrow]]
```

```sh
kala transact varrow.kala scene.edn --json
kala repl varrow.kala --write
```

The three ops commit together at tx#13: Nerys is created, and two facts land at
800. JSON reports `changed: 2`, the new entity and edge IDs, and each fact's
`entries`. Save these entry IDs when you want to correct an edit later. Creating
an entity adds no fact entries, so its op reports `changed: 0` and its entity ID.

In the REPL, ask:

```clojure
[:find ?n ?p :at 800 :where [:nerys :name ?n] [:nerys :located_in ?p]]
```

The row is `"Nerys"  varrow`. `.quit` releases the writer lock. While this session
is open, other writers are blocked. Queries see commits immediately. A failed op
aborts its whole transaction; the error names the op, and the REPL stays open.

You can enter that same transaction vector directly in the write REPL. Use
`.json on` for JSON responses. Ticks default from the op's `:at`, then the form's
`:at`, then session `.at`, then the world's latest tick at transaction start.
Always check the reported tick, especially when writing a prequel. Repeating an
already active fact is `0 changed`; asserting and retracting the same value at
the same tick is an error. Use finer ticks or `correct` to revise that moment.

For a move, retract the previous place and assert the next in one transaction.
If you only add the new place, both remain true: Kala accepts the draft and
reports an overlap warning. Ending the overlap later reports a resolved warning.
An optional `:precision :year` annotates a tick without changing its ordering.

## Ask questions about the sample world

Open a read-only session and pin the original fixture canon so your new scene
does not change the following reference answers:

```sh
kala repl varrow.kala
```

```text
.canon 12
```

Paste these ten queries into that session. An explicit `:canon` overrides the
session default. The complete text responses below are checked by
`crates/kala-cli/tests/guide.rs`. The queries also work with `kala query` on a
freshly loaded fixture.

1. Where was Edwin, and what was his name?

```clojure
[:find ?p ?n :at -260 :where [:edwin :located_in ?p] [:edwin :name ?n]]
```

```text
query at tick -260, canon tx#12: 1 row
  ?p  ?n
  varrow  "Edwin"
```

2. Who were Edwin's allies?

```clojure
[:find ?a :at -210 :where [:edwin :ally_of ?a]]
```

```text
query at tick -210, canon tx#12: 2 rows
  ?a
  greg
  tomas
```

3. Follow chains of alliances with a recursive rule.

```clojure
[:find ?b :at 600
 :rules [[(allied ?a ?b) [?a :ally_of ?b]]
         [(allied ?a ?b) [?a :ally_of ?x] (allied ?x ?b)]]
 :where (allied :edwin ?b)]
```

```text
query at tick 600, canon tx#12: 4 rows
  ?b
  edwin
  mara
  lisa
  tomas
```

4. Read Greg's location change points.

```clojure
[:find ?t ?p ?op :where [:greg :located_in ?p ?t ?op]]
```

```text
query at tick 700, canon tx#12: 5 rows
  ?t  ?p  ?op
  -100  carnath  :assert
  150  carnath  :retract
  200  varrow  :assert
  250  varrow  :retract
  601  ashford  :assert
```

5. Read the draft where Greg was in two places.

```clojure
[:find ?p :at 601 :canon 8 :where [:greg :located_in ?p]]
```

```text
query at tick 601, canon tx#8: 2 rows
  ?p
  varrow
  ashford
```

6. Read alliances before the retcon.

```clojure
[:find ?a :at -195 :canon 11 :where [:edwin :ally_of ?a]]
```

```text
query at tick -195, canon tx#11: 3 rows
  ?a
  greg
  mara
  tomas
```

7. Read the corrected canon.

```clojure
[:find ?a :at -195 :canon 12 :where [:edwin :ally_of ?a]]
```

```text
query at tick -195, canon tx#12: 2 rows
  ?a
  mara
  tomas
```

8. Find an alliance that ended between two ticks.

```clojure
[:find ?a :at -210 :where [:edwin :ally_of ?a] (not (at -200 [:edwin :ally_of ?a]))]
```

```text
query at tick -210, canon tx#12: 1 row
  ?a
  greg
```

9. Count distinct visitors to Varrow.

```clojure
[:find (count-distinct ?c) :where [?c :located_in :varrow ?t :assert]]
```

```text
query at tick 700, canon tx#12: 1 row
  (count-distinct ?c)
  10
```

10. Find overlapping intervals, not just matching change ticks.

```clojure
[:find ?c ?both
 :where (during [:edwin :located_in :varrow] ?ie)
        (during [?c :located_in :varrow] ?ic)
        [(!= ?c :edwin)]
        [(intersect ?ie ?ic) ?both]
        [(not-empty ?both)]]
```

```text
query at tick 700, canon tx#12: 5 rows
  ?c  ?both
  tomas  {[-300, -250)}
  hild  {[-270, -250)}
  osric  {[-300, -250)}
  v01  {[-300, -250)}
  v15  {[-300, -250)}
```

`.quit` ends the session. A history tick variable sees only change points;
for "at the same time", use `during` and `intersect` as in the last example.
`or` combines branches with the same variable set; `(and ...)` groups multiple
clauses inside a branch. A sixth history position binds precision as a keyword:
`[?e :name ?v ?t ?op ?prec]`. `:exact` is the default precision.

## Correct a mistake and read the older draft

Save `leave.edn`:

```clojure
[:retract :nerys :located_in :varrow :at 850]
```

```sh
kala transact varrow.kala leave.edn --json
```

Save the returned op's entry ID as **N** and transaction number as **C** (C is 14
if you followed the steps exactly). Suppose Nerys really left at 830. Put this in
`correct.edn`, replacing N with that numeric entry ID:

```clojure
[:correct N :replace :at 830]
```

```sh
kala transact varrow.kala correct.edn --json
```

This reports tick 830 and `changed: 2`: one marker at the old key and one revised
fact at the new key. The original log entry is preserved. `entry#N` is also
accepted wherever the numeric entry ID goes. `:delete` corrects away an entry;
`:replace` may also change `:value`, `:op`, or `:precision`.

Read before the correction (replace C with the saved number):

```sh
kala query varrow.kala '[:find ?p :at 840 :canon C :where [:nerys :located_in ?p]]'
```

The row is `varrow`. Read the latest canon:

```sh
kala query varrow.kala '[:find ?p :at 840 :where [:nerys :located_in ?p]]'
```

There are no rows: the corrected departure is already in effect at 840.
A write always targets the latest canon. In write mode `.canon` and `.reload`
are refused; an explicit `:canon` in a query can still inspect an older draft.

## Check the world

```sh
kala doctor varrow.kala
```

The guide's world has no integrity errors. Doctor reports any remaining schema
warnings; it never repairs facts or decides whether your story makes sense.

For scripts, a file can contain several transaction vectors. Each commits in
order, and the first failure stops the run while preserving earlier commits.
Output identifies each successful form. `--dry-run` previews every form against
the original world independently, reports warnings, and leaves the log unchanged;
preview IDs are provisional and later previews cannot use earlier previews' new
entities. `--json` produces one document per form. Input is UTF-8, limited to
65,536 bytes, 256 forms, 256 ops per form, and 32 nested brackets. Strings,
escapes, commas and `;` comments follow the same syntax as queries.
