# Expression and row type system implementation plan

Status: implemented. This document is the milestone record of the expression
type system redesign; the resulting behavior is specified in
[type-system-design.md](type-system-design.md). Backward compatibility was not
required and the legacy paths were removed.

It supersedes the earlier implicit-field-lambda design. The milestones
describe implementation dependencies, not a compatibility period. The
parser/checker/compiler contract was replaced together, so no permanent
compatibility mode ships.

## 1. Intended result

Fields become first-class, row-polymorphic expressions. Constants lift
automatically. Records and lists remain ordinary values. Query operations
consume expressions or records of expressions. `select` additionally accepts an
ordinary function that transforms its symbolic column record into a projection.

```sagate
users : query { id = int, name = string, age = int, active = bool } =
  table "public" "users"

minimum_age = 18
adult = .age >= minimum_age

report = users
  & where (adult && .active)
  & select { id = .id, label = upper .name, next_age = .age + 1 }
  & where (.next_age >= 21)
  & order [asc .label]
  & limit 10
```

The pipeline remains ordinary function application. Every consumer unifies an
expression's required row with its actual input relation. No pipeline-specific
name scope or parser-generated lambda is necessary.

The agreed design is:

- One new expression type constructor, `expr phase inputRow valueType`.
- Three phases: `row`, `aggregate`, and `window`.
- `.field` denotes a field expression with an open-row requirement.
- `.<field` and `.>field` denote the two sides of a join predicate.
- Scalar constants, including named constants, lift automatically when an
  expression is expected.
- Only `row -> window` is an implicit phase promotion.
- Projections are ordinary records. There are no `Agg`, `Projection`,
  `AggProjection`, or `WindowProjection` primitive type constructors.
- `pick`, `omit`, and `rename` are ordinary row transformers, usable through
  `select (pick ...)`, `select (omit ...)`, and `select (rename ...)`. They share
  `mapKey`, extended with key dropping; no separate selection row constructors
  are needed. `mapValue` continues to preserve labels and field count.
- `select` accepts either a projection record or a function transforming its
  symbolic input-column record into a projection record.
- `group` and aggregate functions explicitly cross from row to aggregate phase.
- Window functions accept row-phase arguments and specifications, and produce
  window-phase expressions.
- The source parser does not infer expression boundaries from field markers.

Decisions made in this plan to make implementation concrete:

| Question | Initial decision |
| --- | --- |
| Source spelling of expression types | `expr p r a`; always spell the phase in annotations |
| Plain scalar values | Retain them for ordinary computation, table names, limits, and frame options |
| Constant lifting | Insert explicit typed lifting nodes at expression expectations, including inside records |
| Nullable predicates | `where` and join predicates require `bool`; use `isTrue` for `maybe bool` |
| Count functions | `count` means `COUNT(*)`; `countOf e` counts non-null values |
| Window construction | Named functions such as `rowNumber`, `sumOver`, and `lag`; no general `over` initially |
| Window selection | Existing `select` accepts window-phase expressions and promotes row expressions |
| Selection helpers | `select` supplies symbolic columns to an ordinary row-transforming function |
| Selection representation | Extend `KeyMap` with `KeepOnly`, `DropKeys`, and `RenameMap`; reuse `mapkey m r` |
| Initial frames | Implement `ROWS`; defer offset `RANGE`, `GROUPS`, and frame exclusion |
| Scalar numeric types | Retain separate `int` and `float` overloads; no implicit numeric conversion |
| Row labels | Canonical unique visible labels; explicit `merge` remains right-biased |
| Column selection | Static label lists; `pick` uses selection order and `omit` preserves input order |
| Missing selected/removed/renamed fields | Reject; open inputs acquire an existence requirement |
| Key mapping and renaming | Simultaneous mapping; reject collisions rather than overwrite values |

All source code below is proposed syntax unless explicitly identified as an
existing API. Mathematical signatures use `forall` and constraints as notation;
the source language does not need a general user-defined typeclass system.

## 2. Current implementation and gaps

The repository currently has a small expression AST, a handwritten parser,
kinded type terms, a checker returning query schemas, and an SQL compiler that
re-inspects source expressions.

| Area | Current implementation | Required change |
| --- | --- | --- |
| Field syntax | `desugar_implicit_lambda` scans markers and generates lambdas | Preserve explicit field-expression nodes |
| Projection arguments | Records/lists can become callbacks depending on position | Always parse ordinary records/lists |
| Join predicates | Two lambdas and special `that` detection | Structured two-input expression environment |
| Open rows | `Row` stores visible fields and an `Extent` flag | Real tail variables or equivalent persistent constraints |
| Key mapping | `mapKey` takes queries; each label maps to another label | Add record mapping and dropping; retain static selectors, ordering, and validity constraints |
| Row unification | Can skip missing fields on open rows | Retain and propagate every field requirement |
| Polymorphism | Environment stores `Type`; freshening and expansion handle some uses | Explicit schemes, generalization, instantiation, and annotation checking |
| Field inference | Some accesses produce `Any` | Fresh constrained variables or a type error |
| Checker output | `HashMap<String, Row>` | Typed/elaborated program plus query schemas |
| Aggregation | `AggregateField` records one operation and an optional field name | Typed aggregate expression trees with grouping metadata |
| SQL lowering | Extracts lambdas and independently infers some field types | Consume the checker's typed relational/scalar IR |
| Windows | No corresponding operations in `ForeignId` | Typed functions, specifications, lowering, and execution tests |

Relevant current files:

- [ast.rs](crates/sagate-core/src/lang/ast.rs)
- [grammar.rs](crates/sagate-core/src/lang/parser/grammar.rs)
- [unification.rs](crates/sagate-core/src/lang/checker/inference/unification.rs)
- [inference expressions](crates/sagate-core/src/lang/checker/inference/expressions.rs)
- [checker validation](crates/sagate-core/src/lang/checker/validation.rs)
- [shared relational metadata](crates/sagate-core/src/lang/relational.rs)
- [SQL expression lowering](crates/sagate-sql/src/compiler/expressions.rs)
- [SQL relational lowering](crates/sagate-sql/src/compiler/relational.rs)

Do not treat the existing design document's descriptions of tail solving and
deferred constraints as evidence that those mechanisms are implemented. The
replacement needs explicit implementations and regression tests.

## 3. Type representation

### 3.1 Kinds and expression types

Retain `Type`, `Row`, `KeyMap`, and `ValueMap`; add `Phase` and `KeyList`.
`KeyList` indexes a statically known ordered list of distinct labels, as
described in section 3.5. It supplies static payloads to key mappers, without
adding selection-specific row constructors or arbitrary dependent types.

```text
expr : Phase -> Row -> Type -> Type
row  : Row -> Type
query : Row -> Type
keys : KeyList -> Type

Phase ::= row | aggregate | window | phaseVariable
```

`row` in the phase position is unambiguous because the constructor's argument
kind is known. The parser handles this fixed argument position; it does not
guess whether an entire expression is a callback.

Add a type representation resembling:

```rust
Type::Expression {
    phase: PhaseTerm,
    input: RowTerm,
    result: Box<Type>,
}

enum PhaseTerm {
    Row,
    Aggregate,
    Window,
    Variable(PhaseVarId),
}
```

Use distinct variable identifiers or tagged keys for value, row, phase, key-list,
and mapper variables. Extend substitution, free-variable collection, occurs checks,
freshening, kind checking, and pretty-printing together. Avoid adding a new
variant to inference while forgetting it in annotation or substitution code.

`expr p r a` is a description of a SQL expression, not an implicit conversion
to or from `row r -> a`. Explicit lambdas remain useful for ordinary functions
that construct and combine expression values.

### 3.2 Rows and actual tails

Represent extensible rows with a tail that participates in substitution:

```text
Row ::= { fields | Tail }
Tail ::= Empty | RowVariable
```

Retain explicit first-order row operations:

```text
merge older newer
mapkey mapper input
mapvalue mapper input
```

Represent selection using `mapkey (keepOnly k) input` or
`mapkey (dropKeys k) input`. Extend the key-mapper term language, not this list
of row constructors.

A workable representation is a field map plus `Empty`/`Variable` tail, inside a
row-term enum that also represents those operations. Preserve presentation order
separately from field lookup/unification order.

Annotations such as `{ id = int }` are closed. Add explicit tail syntax:

```sagate
expr row { age = int | rest } bool
```

Canonical rows have unique visible labels. Track lacks constraints on tails so
a tail cannot hide another occurrence of a visible label. Normalize concrete
right-biased merges to unique visible fields. For opaque merges, preserve the
operation and its constraints until enough information is available.

The existing `Extent` flag can remain temporarily at API boundaries, but it
must not act as the inference representation of an unknown tail.

### 3.3 Field expressions

The ordinary field rule is:

```text
.field : forall a tail. expr row { field = a | tail } a
```

Each occurrence initially gets fresh variables. Its SQL column value type must
eventually be supported by the backend. Unconstrained variables are not `Any`.

Examples:

```text
.age >= 18
  : expr row { age = int | tail } bool

(.age >= 18) && .active
  : expr row { age = int, active = bool | tail } bool
```

Repeated access to one visible label shares its type after input-row
unification. Thus `(.id == 1) && (.id == "x")` is an error.

Closed input schemas reject missing fields. Open source schemas may accumulate
requirements, but unresolved column types or scalar overloads must be diagnosed
before an executable query requiring those types is emitted. Generalized helper
definitions may retain legitimate row/type variables.

### 3.4 One parameterized value mapper

Extend the existing closed value-mapper language with:

```text
ValueMap ::= ... | ExprOf(phase, inputRow)
```

Source spelling in mapper position:

```text
mapvalue (expr p r) s
```

This is a supported first-order mapper constructor, not unrestricted partial
application of arbitrary type-level functions.

For a concrete row:

```text
mapvalue (expr p r) { id = int, name = string }
  = { id = expr p r int, name = expr p r string }
```

Support structural forward mapping and inference of the output value row from
a concrete record of expressions. For an unknown record shape, retain a mapping
constraint. Do not require the record to be syntactically written inline.

`AggProjection` and `WindowSpec` in explanations are descriptive abbreviations,
not additional nominal type constructors.

### 3.5 Row helpers for select

These operations transform an ordinary record's shape and preserve the complete
types of retained field values. Their main query use is to transform the
symbolic record that `select` supplies, whose fields contain column expressions.
Implement all three through a shared record-level `mapKey`. The helpers do not
take a query argument or rewrite an expression's required input row. Section 3.6
defines the `select` overload that makes this work.

Proposed surface examples:

```sagate
public_users = users
  & select (omit ["password_hash"])
  & select (rename { display_name = "name" })
  & select (pick ["id", "name"])

brief = users
  & select (pick ["id", "display_name"] >>> rename { display_name = "label" })
```

The query examples assume input schemas containing the named columns. A field
introduced by the rename projection becomes available to subsequent pipeline
stages, and its old name disappears.

**One key-mapping operation.** Generalize the existing key mapper from always
returning a label to either retaining a field under a label or dropping it:

```text
mapLabel : KnownKeyMapper -> Label -> Keep Label | Drop

KeyMap ::= ...existing mapper forms...
         | KeepOnly KeyListTerm
         | DropKeys KeyListTerm
         | RenameMap { oldLabel -> newLabel }
```

`Keep` and `Drop` describe an internal mapping decision, not SQL values or new
source types. Existing total mappers return `Keep mappedLabel`. `KeepOnly`
retains listed labels unchanged and drops all others. `DropKeys` drops listed
labels and retains all others. `RenameMap` changes listed source labels and
retains unmapped labels. Retained fields keep their complete value types.

These are closed, compiler-known mapper forms, not arbitrary runtime functions
over strings. Source type spellings for the first two are `keepOnly k` and
`dropKeys k`; both have kind `KeyMap`. All three produce rows through the
existing `mapkey m r` constructor. Do not add `RowExpr::Pick` or `RowExpr::Omit`.

The shared operation validates required source fields and unique output labels.
It also tracks output order: `KeepOnly` follows its static key list; `DropKeys`,
finite renaming, and existing total mappers preserve retained input order.
A per-label filter alone cannot implement the chosen `pick` ordering contract.
Keep that ordering metadata separate from structural row equality.

`mapValue` remains a value-type mapping that preserves labels and field count.
Neither nullable fields nor special scalar values represent removed columns.

**Static specifications.** The output cannot have a precise static row type
when the column names are an arbitrary runtime `list string`. Introduce one
small witness type, `keys k`, using `KeyListTerm::Known(labels)` or a key-list
variable as its index. `keepOnly` and `dropKeys` wrap this index in a
`keymapper` witness. Reuse `keymapper m` directly for finite renaming and
existing mappers such as `snake`.

At an expected `keys k`, a compile-time-known list of strings elaborates to a
key-list witness with that exact index. At an expected `keymapper m`, a
compile-time-known record of string destinations elaborates to a finite rename
mapper. Record field names are the old names; string values are the new names.
Named constants and reusable specifications work:

```sagate
public_columns = ["id", "name"]
renames = { display_name = "name" }

only_public = pick public_columns
normalize_names = rename renames
public_mapper = keepOnly public_columns
also_public = users & select (mapKey public_mapper)
```

This is a narrowly defined compile-time configuration elaboration rule, separate
from SQL constant lifting. It needs no new expression grammar and no explicit
`keys(...)` wrapper in ordinary code. Do not give a constructor the unsound
signature `list string -> keys k` with unconstrained universally quantified `k`:
the index must be derived from, and agree with, the actual static list. Reject
configurations whose contents cannot be determined before SQL generation.
Generic helpers may take `keys k` or `keymapper m` parameters and retain symbolic
indices; actual executable uses must supply compatible static witnesses.
For example, a helper can accept `keys k`; a parameter annotated only as
`list string` cannot acquire a value-dependent output schema inside a universal
function definition. Typechecking that function must not depend on which
constant a later caller happens to pass.

Allow `keys ["id", "name"]` in source type annotations and `keys k` for a
symbolic index. Preserve the index through schemes, imports, and aliases.

**General signatures.** Use indexed mapper constructors and one checked
record-mapping primitive:

```text
keepOnly : keys k -> keymapper (keepOnly k)
dropKeys : keys k -> keymapper (dropKeys k)

mapKey : ValidKeyMap(m, r) =>
         keymapper m -> row r -> row (mapkey m r)

pick : HasKeys(k, r) =>
       keys k -> row r -> row (mapkey (keepOnly k) r)
omit : HasKeys(k, r) =>
       keys k -> row r -> row (mapkey (dropKeys k) r)
rename : ValidKeyMap(m, r) =>
         keymapper m -> row r -> row (mapkey m r)
```

For `KeepOnly` and `DropKeys`, `ValidKeyMap` reduces to `HasKeys` once the witness
establishes distinct labels. Finite renaming additionally checks output
collisions. Retain unresolved validity constraints in schemes.

The record helpers are ordinary library definitions with the signatures above:

```sagate
pick = labels => mapKey (keepOnly labels)
omit = labels => mapKey (dropKeys labels)
rename = mapper => mapKey mapper
```

The helper signatures select the record overload of `mapKey`. A static
record such as `{ name = "label" }` elaborates to a `RenameMap` witness at its
`keymapper m` expectation; no extra rename constructor call is required. Existing
mappers such as `snake` also work, subject to the same no-collision requirement.
No query overloads of these three helpers are needed.

Retain the existing query form of `mapKey` as a convenience with the qualified
signature `ValidKeyMap(m, r) => keymapper m -> query r -> query (mapkey m r)`.
Implement it through symbolic-column selection and the same record primitive;
it must not have separate field-removal, collision, or ordering rules.

**Pick.** All selected fields must exist. Bind their types with `HasField` and
return a closed row containing exactly those fields, even if the input has an
open tail. Output field order follows the static key list. For example:

```text
pick ["id", "name"] :
  forall a b tail.
  row { id = a, name = b | tail } -> row { id = a, name = b }
```

Reject duplicate selector names instead of silently changing multiplicity.
Picking no fields produces an empty record.

**Omit.** Require each removed field to exist and preserve every other field,
including an open tail. For one field:

```text
omit ["age"] :
  forall a tail.
  row { age = a | tail } -> row tail
```

Canonical-row lacks constraints ensure `age` cannot survive inside `tail`.
Preserve the relative order of retained fields. Removing no fields is identity;
duplicate selectors and missing fields are errors. Do not silently turn an
unknown remaining tail into a closed empty row.

**Rename.** A finite rename is simultaneous: inspect all old labels before
producing the new ones. Require every source to exist, require distinct
destinations, and reject a destination that collides with an unchanged field.
For one non-identity rename:

```text
rename { name = "label" } :
  forall a tail. LacksField(tail, "label") =>
  row { name = a | tail } -> row { label = a | tail }
```

The source tail already lacks `name` by canonical row formation. For multiple
renames, peel off all source fields and require the residual tail to lack every
destination. This permits swaps while preventing overwrites:

```sagate
{ a = 1, b = "x" } & rename { a = "b", b = "a" }
# Result: { b = 1, a = "x" }
```

Renaming a field to itself is valid, but still requires that field to exist.
An empty rename map is identity. Preserve each source field's position while
changing its label. Exact Unicode spelling remains the label identity rule.

For arbitrary existing key mappers, retain a `ValidKeyMap(m, r)` constraint.
Identity/prefix/suffix can have known injectivity facts. For finite maps, reduce
validation to source-presence and tail-lacks constraints. For a mapper such as
`snake`, whose collisions depend on the actual labels, defer validation until
the relevant schema is known. Apply this checked behavior to `mapKey` itself,
including its query form. Replace the older collision behavior during the
breaking migration; only an explicit `merge` remains right-biased.

**Normalization and composition.** `mapkey (keepOnly k) r` can normalize to a
closed row after the requested fields are established, without enumerating the
remaining open tail. `mapkey (dropKeys k) r` peels off the required fields and
preserves the residual tail. Finite renaming rewrites known sources while
retaining destination-lacks constraints. Unknown mapper indices remain deferred
`mapkey` terms with `ValidKeyMap` constraints until instantiated.

Function composition produces nested mappings. For example, pick followed by
rename has output `mapkey m (mapkey (keepOnly k) r)`, with
`HasKeys(k, r)` and `ValidKeyMap(m, mapkey (keepOnly k) r)`. Check each stage
against its own input, preserving ordering and validity evidence. Do not fuse
mappings in a way that hides a missing field or a collision at an intermediate
stage, even if a later mapping would remove the affected columns.

**Records of expressions.** These are ordinary records too:

```sagate
projection = { id = .id, label = upper .name, active = .active }
brief = projection & pick ["id", "label"]
report = users & select brief
```

Picking/omitting fields preserves their individual expression types; the later
consumer unifies the input requirements of the fields that remain. Renaming
`label` to `title` changes only the output key. It must not rewrite `.name`,
change an expression's phase, or pretend its input requirement was renamed.
Likewise, if field input rows were already unified by a prior annotation,
dropping a record field must not erase those established constraints.

For a valid key mapper and a uniform record of expressions, the structural
relationship is:

```text
mapkey m (mapvalue (expr p input) output)
  = mapvalue (expr p input) (mapkey m output)
```

This includes `KeepOnly`, `DropKeys`, and valid renaming. The input row `input`
is unchanged on both sides; mapper validity and presentation order are retained.

**SQL lowering.** `select` applies the transformer to its symbolic input record
and lowers the resulting expression record to columns and aliases. Preserve
values, nullability, row multiplicity, and required pipeline boundaries. Picking
known fields can close an open input schema. Omitting or renaming while keeping
all other columns may require a concrete schema to enumerate those columns; if
it is still unknown, request a schema annotation. Do not fabricate a wildcard
exclusion/rename feature unsupported by the target dialect.

Pure empty records are valid. For an executable query with no visible output
columns, initially issue a precise unsupported-empty-projection diagnostic;
portable SQL unit-row encoding is separate work. Never silently retain omitted
columns or expose an undocumented dummy column as the user's schema.

### 3.6 Selecting with an ordinary row transformer

Keep the direct projection-record form and add a function form:

```sagate
users & select { id = .id, name = .name }
users & select (pick ["id", "name"])
```

Define the following abbreviation only for explanation:

```text
Columns r = row (mapvalue (expr row r) r)
```

If the input schema is `{ id = int, name = string, age = int }`, `select`
supplies a symbolic record with the same labels:

```text
{
  id   : expr row r int,
  name : expr row r string,
  age  : expr row r int
}
```

Its values are bound column-expression recipes, not database row values. Applying
`pick ["id", "name"]` returns the first two of those expression values. Applying
`rename { name = "label" }` changes a record key while retaining the expression
that reads the original `name` column. Ordinary function composition therefore
works without any new projection-builder type:

```sagate
public_projection = pick ["id", "name"] >>> rename { name = "label" }
report = users & select public_projection
```

Use a compiler-owned constraint `Projectable(targetPhase, input, raw, output)`
to describe structural checking of a projection record. For each raw field:

- An ordinary supported scalar value of type `a` lifts to an expression.
- An `expr p input' a` unifies `input'` with `input` and requires promotion from
  `p` to `targetPhase`.
- The output field has type `a`, with the same label.
- Functions, queries, and backend option values are rejected as output columns.

Retain the relevant mapping constraints for unknown row shapes. This is the
same checking already required for named, mixed constant/expression records;
it is not a new public type constructor.

The two source-facing overloads can then be specified precisely as:

```text
select : Projectable(window, r, t, s) =>
         row t -> query r -> query s

select : Projectable(window, r, t, s) =>
         (Columns r -> row t) -> query r -> query s
```

After elaboration, both produce the existing uniform core projection type
`row (mapvalue (expr window r) s)`. In particular, do not require the supplied
function to already return a uniformly window-phase record. Apply it first,
then check/coerce its result. This supports ordinary `pick` results and custom
functions returning records that mix row expressions, window expressions, and
constants, without introducing implicit subtyping of function arrows.

Implement the function overload as follows:

1. Connect the actual query schema to `r` through ordinary application typing.
2. Instantiate the transformer's scheme and check its input against `Columns r`.
3. Apply the typed transformer to a symbolic record view of that query input.
4. Check the resulting record with `Projectable(window, r, t, s)`, retaining
   explicit evidence for scalar lifting and row-to-window promotion.
5. Produce the same checked `Select` node used by direct record projections.

The symbolic view can supply a requested field without enumerating an unknown
tail. Operations that retain the whole remaining input need its labels before
materialization. Preserve constraints until that information is available;
never create arbitrary fields merely to satisfy an expected output type.

Distinguish the two overloads by the argument's type, not its source syntax or
the names `pick`, `omit`, and `rename`. Named, imported, composed, and user-defined
row transformers must follow the same path. If a generic definition leaves the
record-versus-function overload ambiguous, require a signature rather than
guessing. No implicit lambdas or bare-column-name scopes are introduced.

Only `select` gains this function overload in the initial design. `where`
continues to require a boolean expression; `agg` continues to require an
aggregate-expression record. General expression-to-function conversions remain
absent.

## 4. Constraints and inference

### 4.1 Constraint forms

Introduce explicit constraints, with source origins:

```text
TypeEqual(a, b)
RowEqual(r, s)
HasField(r, label, a)
LacksField(r, label)
PhaseCanPromote(p, q)
MappedRow(mapper, input, output)
HasKeys(keys, input)
ValidKeyMap(mapper, input)
Projectable(targetPhase, input, rawProjection, output)
OverloadChoice(candidates, argumentTypes, resultType)
SqlValue(a), Groupable(a), Orderable(a)
```

Some forms may reduce immediately to unification rather than becoming stored
nodes. `SqlValue`, `Groupable`, and `Orderable` are compiler-owned predicates over
the supported scalar types; this does not introduce open typeclass resolution.
`HasKeys` expands to field-existence constraints when the key list is known.
`ValidKeyMap` preserves source-presence and destination-collision requirements
through polymorphic definitions. For `KeepOnly`/`DropKeys`, reduce it to the
corresponding `HasKeys`; finite renaming also generates tail-lacks constraints.
The shared `mapkey` row term normalizes structurally when possible and otherwise
participates in deferred row equality. Successful output normalization must not
discard unresolved validity requirements on its input.
`Projectable` carries the structural checking and coercion evidence shared by
direct projections and the results of row-transforming functions.

Attach an origin to each requirement, so an error can name both incompatible
uses of `.id` or point to the earlier projection that removed a required field.

### 4.2 Row unification

Implement the standard extensible-row algorithm:

1. Apply current substitutions and normalize available concrete row operations.
2. Unify the types of shared labels.
3. Collect labels present only on each side.
4. Absorb unmatched labels into the opposite open tail.
5. When both sides are open, connect their residual tails through a fresh shared
   tail, preserving lacks constraints.
6. Reject an unmatched required label against an empty/closed tail.
7. Run kind and occurs checks before every variable binding.

For example:

```text
{ age = int | alpha } ~ { active = bool | beta }

alpha = { active = bool | gamma }
beta  = { age = int | gamma }
```

Never combine expression requirements using right-biased `merge`. Requirements
for the same input field must agree.

`mapkey`, `mapvalue`, and `merge` are not generally injective; key dropping
particularly prevents recovering a unique input row from an output row.
Do not guess an inverse or discard equality when their inputs are unknown.
Solve supported structural cases and preserve the rest as deferred constraints. Before emitting
an executable query, report unresolved constraints needed for that query's
schema or SQL operations. A generalized definition can carry constraints into
its scheme.

### 4.3 Phase compatibility

Use this complete initial promotion relation:

| Source | Target | Result |
| --- | --- | --- |
| row | row | identity |
| row | window | explicit promotion evidence |
| aggregate | aggregate | identity |
| window | window | identity |
| any other pair | | error |

This is not a total order. In particular, row and aggregate have no common
permitted result phase.

A scalar operation has one shared result phase:

```text
_+_ : expr p r int -> expr p r int -> expr p r int
```

Do not bind `p = row` permanently just because the first argument is row-phase.
Record lower-bound/compatibility constraints until all arguments and the
expected result are known. Row plus window yields window regardless of operand
order. Two row operands choose row when unconstrained by a larger expectation.

Implement a small solver over the three concrete phases and phase variables.
Generalized variables retain relevant constraints; annotations with quantified
phase variables must be checked for every allowed instantiation. Produce
explicit promotion evidence for elaboration. Do not silently promote phases
inside unrelated containers or function arrows.

### 4.4 Schemes and annotations

Replace the inference environment's bare types with schemes:

```text
Scheme = forall quantifiedVariables. constraints => type
```

- Generalize only variables not free in the lexical environment.
- Keep lambda parameters monomorphic within their body.
- Instantiate all quantified variables and constraints freshly at each use.
- Check universal annotations using rigid/skolem variables rather than treating
  an annotation as a source of permissive fresh unknowns.
- Include row tails, lacks constraints, phases, key-list indices, key-mapper
  validity, and parameterized mappers in generalization and instantiation.
- Keep constraints attached to named and imported predicates/projections.
- Resolve aliases through ordinary binding semantics; do not copy source ASTs
  as a substitute for polymorphic inference.

Examples that must work include a reused `adult = .age >= 18` predicate on two
different compatible schemas and an imported projection used independently at
multiple call sites. A lambda parameter used once as `int -> int` and once as
`bool -> bool` must not accidentally become polymorphic.

### 4.5 Bidirectional checking and automatic lifting

Use inference to discover an expression's type and checking when a consumer
provides an expected type. At an expectation `expr p r a`, an ordinary supported
scalar value of type `a` elaborates to a typed `Pure` node.

```sagate
minimum_age = 18
adult = .age >= minimum_age
constant_predicate = true : expr row r bool
```

Constants need not carry row requirements. The lifted value can be used in any
input row and in any phase chosen by its expectation.

Lifting is one-way and does not apply to an already row-dependent expression
as if it were an ordinary scalar value. There is no general `Expr -> scalar`,
`row -> aggregate`, or nested-`Expr` escape hatch. SQL-embeddable scalar types
exclude function values, queries, expression objects, and compiler option
objects such as sort keys.

Check records and lists structurally when their expected types require lifted
elements. This must also work for named records, not only literal syntax:

```sagate
projection = { label = "all", id = .id }
report = users & select projection
```

Elaborate an ordinary constant field into `Pure`, preserve its label, and unify
the other fields' row requirements. A typed record-coercion node can represent
this transformation without evaluating a shared binding multiple times.

Plain scalar arguments remain plain where required: `table "public" "users"`,
`limit 10`, and frame offsets do not receive accidental expression wrappers.
Literal `null` requires a nullable expected type; unresolved null payload types
must not become `Any`.

### 4.6 Overloads and scalar functions

Keep finite numeric overloads. Add phase-polymorphic expression signatures to
SQL scalar operators and functions. Preserve plain scalar overloads only where
the language has a corresponding ordinary scalar implementation.

Resolve exact applicable overloads before candidates requiring scalar lifting.
Defer choices until enough argument/result information is available. If several
non-equivalent candidates remain, report ambiguity or require an annotation;
do not choose according to declaration order. Generic definitions whose numeric
or plain-versus-expression overload remains unresolved need explicit signatures
or explicit overload cases.

The compiler must record the chosen binding/overload and coercions. SQL lowering
must not repeat overload resolution from the function's source spelling.

## 5. Surface parsing and name resolution

Add source AST variants with spans for ordinary, left, and right field
expressions. A path representation is useful for diagnostics, but source input
side and column label must remain distinct pieces of information.

Parse these as fixed atoms:

```sagate
.name
.<user_id
.>id
```

Remove `desugar_implicit_lambda`, `implicit_field_markers`,
`rewrite_implicit_fields`, `implicit_row_lambda`, and lambda-depth behavior used
only to support field-marker sugar.

Parentheses always group. Records and lists always construct values. `that`
becomes an ordinary identifier. Functions receiving SQL expressions do not
accept row callbacks automatically; the explicit transformer overload of
`select` follows section 3.6 and receives expression-valued fields.

Separate postfix record access from function application. `upper person.name`
must parse as `upper (person.name)`. Preserve the small lexical distinction
between adjacent `person.name` access and a field-expression argument in
`upper .name`; document and test this rather than inferring a lambda scope.

Add the fixed `expr` type constructor, phase terms, explicit row tails, and
`ExprOf` mapper spelling to the type grammar. Field labels after a selector are
not module/global variable references. Update module name resolution and
capture-avoiding substitution to traverse the new nodes without rewriting their
column names. Ordinary explicit lambdas and module visibility continue working.
Add key-list witness types and the `keepOnly`/`dropKeys` mapper terms to the type
grammar. They occupy the existing mapper position in `mapkey`; there are no new
selection row constructors. Value-level helpers, mapper constructor calls, and
list/record configurations use the existing expression grammar.

## 6. Query and join signatures

The following show core signatures after projection elaboration. The two
source-facing `select` overloads are specified in section 3.6:

```text
where : expr row r bool -> query r -> query r

select : row (mapvalue (expr window r) s) -> query r -> query s
agg    : row (mapvalue (expr aggregate r) s) -> query r -> query s

asc, desc : Orderable a => expr p r a -> expr p r direction
order : list (expr row r direction) -> query r -> query r
limit : int -> query r -> query r
```

Projection output fields must be SQL column value types. This excludes
projecting a sort-direction wrapper as if it were a database column. `asc` and
`desc` retain the underlying expression's type and ordering metadata in the IR.

For joins, `JoinInput l r` below is a document abbreviation for the structural
row `{ left = row l, right = row r }`, not another value type constructor:

```text
.<field : expr row (JoinInput { field = a | tail } r) a
.>field : expr row (JoinInput l { field = a | tail }) a

inner : query r
     -> expr row (JoinInput l r) bool
     -> query l
     -> query (merge r l)
```

The source prelude can write the structural row explicitly. Internally, keep
left/right input identity in the selector and resolved column reference;
matching schemas or equal column names must not erase side information.

```sagate
report = orders & inner users (.<user_id == .>id)
```

Implement `onLeft` and `onRight` as expression-environment adapters so ordinary
reusable predicates can be used on either input. Their elaboration must
substitute the environment through the expression, including lifted constants,
without changing its phase or scalar type.

Outer joins check `ON` against the original input rows, then apply idempotent
nullability widening to the appropriate side of the output. Preserve the
current left-wins collision policy, equivalent to `merge right left`. It is an
output policy, never a policy for combining predicate requirements.

Expressions in a join predicate must use the structured input consistently.
A plain `.id` cannot silently mean either side. Project or rename inputs before
a join when the output needs both same-named columns.

## 7. Aggregation

Use phase transitions instead of separate aggregate/group result wrappers:

```text
group   : Groupable a => expr row r a -> expr aggregate r a
count   : expr aggregate r int
countOf : SqlValue a => expr row r a -> expr aggregate r int

sum : expr row r float -> expr aggregate r (maybe float)
sum : expr row r int   -> expr aggregate r (maybe int)
```

Define corresponding nullable-input cases. `avg`, `min`, and `max` also account
for empty input and all-null input. Use a nullability normalization operation
so repeated widening does not produce distinct nested SQL nullability levels.
Apply that normalization to SQL-value transformations, not to arbitrary nested
`maybe` values in ordinary functional code.
Backend numeric result widths and casts must match the advertised scalar types.

`sum0` can be a library definition using `coalesce`, with separate int/float
cases. SQL three-valued comparisons produce nullable booleans where appropriate;
`isTrue` explicitly converts one to a filtering predicate. Do not silently type
a nullable comparison as non-null `bool`.

```sagate
totals = orders
  & agg {
      customer = group .customer_id,
      revenue = sum .amount,
      orders = count
    }
  & where (.orders >= 5)
```

The aggregate IR must preserve both the typed scalar expression and the set of
grouping-key expressions contributed by `group` nodes. Do not reduce an output
field to a single aggregate operation and field name.

Required compositions include:

```sagate
count + 1
upper (group .country)
group (upper .country)
coalesce (sum .amount) 0.0
```

The two country expressions group differently. Preserve the original expression
marked by each `group` node. Deduplicate grouping keys only by resolved,
semantics-preserving expression identity; never by displayed text alone.

`agg` with no grouping keys means one global group, including on empty input.
A projection containing only constants must still have this cardinality.
Force a genuine aggregate query using an internal aggregate anchor or a
supported empty grouping set, then project away the anchor if necessary.
With grouping keys, empty input yields zero groups.

Reject ungrouped fields in `agg`, nested aggregates, aggregate predicates in
`where`, and window expressions inside aggregate arguments. Aggregation of a
previous aggregate's projected column is legal in a later pipeline stage.

## 8. Windows

### 8.1 Function contracts

Initial window functions:

```text
rowNumber, rank, denseRank : WindowSpec r keys -> expr window r int
sumOver : WindowSpec r keys -> expr row r float -> expr window r (maybe float)
avgOver : WindowSpec r keys -> expr row r float -> expr window r (maybe float)
lag, lead : WindowSpec r keys -> expr row r int -> expr window r (maybe int)
```

These show representative scalar overloads. Supply supported int/float and
nullable-input cases, and preserve nullability for lag/lead. Start lag/lead with
offset one and no default; additional offset/default APIs are later extensions.

Do not add `over : expr aggregate r a -> ...`. That would also accept grouping
keys and arbitrary completed aggregate calculations. Named window functions
directly construct valid window-function calls.

### 8.2 Ordinary specification records

`WindowSpec r keys` is documentation shorthand for an ordinary record:

```text
{
  partition = row (mapvalue (expr row r) keys),
  order = list (expr row r direction),
  frame = frame
}
```

Partition keys use a record so different scalar key types can coexist without
`Any` or heterogeneous-list magic. Labels identify key entries in diagnostics;
the SQL partition list uses their values. Constrain each key to a supported
partition/grouping value type.

For the first implementation, require all three fields and provide normal
library constructors/helpers for common specifications. Avoid optional record
fields or context-dependent defaults as a hidden type-system feature.

`frame` and its bounds are small SQL option values, not new expression or
projection type families. Provide ordinary constructors such as
`wholePartition`, `rowsBetween`, `unboundedPreceding`, `currentRow`, and
`preceding n`/`following n`. Their integer arguments are ordinary values known
to the query compiler, not row expressions.

```sagate
recent_first = {
  partition = { customer = .customer_id },
  order = [desc .created_at, asc .id],
  frame = wholePartition
}

recent = orders
  & select { id = .id, position = rowNumber recent_first }
  & where (.position <= 3)
```

Require ordering for the initial row-number/ranking and lag/lead APIs; it need
not be unique, but tests must use a tie-breaker when asserting exact row order.
`sumOver`/`avgOver` can use an empty order list for whole-partition aggregation.

Ranking and lag/lead lower without a frame clause; accept their canonical
`wholePartition` option and reject irrelevant custom frames. Aggregate window
functions lower the selected frame explicitly so backend defaults cannot
silently change results.

### 8.3 Phase and SQL-boundary rules

All partition keys, order keys, and window input expressions must be row-phase.
Validate nested specifications and arguments, not only the outer result phase.
Reject a window result used inside another window at the same stage.

`select` promotes row fields to window phase when required, while its output
schema contains plain SQL value types. A later `.position` is therefore a new
row-phase reference to the selected column.

The compiler must preserve the boundary between a window-producing projection
and a subsequent filter. CTE construction alone is not evidence that optimizer
passes preserve it. Test the optimized SQL by executing a top-N-per-partition
query, and gate transformations that move predicates across this boundary.

Aggregates followed by windows use the same mechanism: aggregate output columns
become ordinary inputs to the following stage. Do not introduce an implicit
aggregate-to-window promotion.

### 8.4 Initial frame coverage

Implement validated `ROWS` bounds, including whole partition and running frames.
Reject negative offsets, impossible bound ordering, and unsupported frame
options before rendering SQL. Empty valid frames are permitted and retain
nullable aggregate results.

Offset `RANGE` needs additional constraints on ordering-key count and type;
`GROUPS` and exclusion clauses also require backend capability checks. Keep them
out of the initial API rather than accepting and ignoring unsupported options.
Verify sqlglot-rust's AST, rendering, and optimizer support in an early spike;
the repository depends on version 0.10.30, but this plan does not assume that
every required window form is already exposed by that version.

## 9. Typed elaboration and SQL lowering

Introduce a typed program between source AST and SQL generation. A possible
pipeline is:

```text
source AST -> name resolution -> inference/checking + evidence
           -> typed expression/query IR -> SQL AST -> validated optimization
           -> dialect rendering
```

The typed representation should retain:

- The inferred type and source span of each relevant expression.
- Explicit `Pure` and row-to-window promotion nodes.
- Selected scalar overload/foreign operation identity.
- Field requirements and resolved input-side/column identities at query use.
- Group keys, aggregate calls, and window calls/specifications.
- Projection schemas and semantic boundaries between relational steps.
- Typed record/list coercions where constants were lifted structurally.
- Static key-list and `KeepOnly`/`DropKeys`/`RenameMap` witnesses, retained
  `ValidKeyMap` evidence, and resolved field selections/aliases/order for the
  shared `mapKey` primitive applied to symbolic input records.
- The selected direct-record or row-transformer overload of `select`, with a
  typed application and result-checking evidence for the latter.

Ordinary lambda application can still be normalized during expression building,
but SQL lowering must not reconstruct implicit lambdas or recover semantics by
matching public function names. Preserve `ForeignId` as the backend operation
identity so aliases, imports, and user shadowing behave consistently.

Register the transformer selection overload separately, for example with
`ForeignId::SelectWith`, and normalize both selection forms to one checked
`Select` node. Its registered type scheme must carry the `Projectable`
constraint linking the transformer result to the query output; an unconstrained
output row variable would be unsound. Backend SQL lowering receives the checked
projection record and does not need another projection syntax or callback type.

Lower `pick`, `omit`, and `rename` through their ordinary library definitions
and the checked `mapKey` operation identity. Do not add helper-specific SQL
dispatch or field-selection IR families. The existing query `mapKey` operation
uses the same symbolic-column mapping and produces the same checked `Select`
node; both mapping overloads validate all source and destination constraints.

An expression helper can remain row-polymorphic until consumed. At each query
use, instantiate its scheme and bind its column paths to that relation's
columns. A reusable expression is a recipe over its required row, not a captured
SQL alias. Self-joins must receive fresh, separate source identities.

Add a checker API such as `check_program -> CheckedProgram`. Existing
`type_check -> query schemas` may become a thin adapter while callers migrate.
Both `compile_with_dialect` and `compile_linked_with_dialect` must compile the
same checked representation. Do not maintain separate inference rules in core
checking and SQL compilation.

Scalar `sql` templates retain explicit signatures and positional substitution.
Their declared scalar/aggregate/window category must agree with the operation
being registered. Validate recognizable aggregate/window syntax in scalar
templates; arbitrary external SQL function declarations remain a trusted
boundary. A user-written signature is not proof of a database function's true
semantics or a table's actual deployed schema.

Audit `mapValue` against real SQL semantics. Nullability widening can preserve
the underlying SQL value; changing a scalar into a list cannot be implemented
by changing only its type. Keep the type-level `list` mapper if useful, but do
not expose relational `mapValue list` as a working conversion until the backend
constructs the corresponding value. Report unsupported conversions explicitly.

## 10. Implementation sequence

The following milestones are ordered by dependency. Each includes implementation
and meaningful checks; completion means its exit criteria hold, not merely that
new enum variants exist.

### M0. Characterize behavior and verify backend capabilities

- Inventory implicit-lambda assumptions in parser, checker, shared relational
  metadata, SQL compiler, examples, and tests.
- Record the existing module visibility, operator override, pipeline-ordering,
  and Unicode behavior that the redesign should retain. Distinguish retained
  merge/join collision policies from the deliberate `mapKey` collision change.
- Run the current workspace checks once and record pre-existing failures.
- Verify SQL AST construction/rendering for grouping expressions, global
  constant aggregation, `ROW_NUMBER`, ordered window sums, and `ROWS` frames.
- Check optimizer behavior on window/filter and aggregate/filter boundaries.
- Select the executable SQL fixture backend: use SQLite through a bundled
  `rusqlite` dev dependency for self-contained core semantic tests, plus AST and
  render checks for other supported dialects. Verify capability restrictions.

Exit: backend gaps and any required dependency change are concrete, with small
reproducers. There is a baseline for the eventual breaking migration.

### M1. Implement row tails, schemes, and the constraint foundation

Primary files: `ast.rs`, `checker/inference/unification.rs`,
`checker/inference/helpers.rs`, and `checker/validation.rs`.

- Add genuine row tails, lacks constraints, and deferred row-operation equality.
- Cover field removal and destination-lacks constraints needed for strict
  omission and collision-free renaming of open rows.
- Introduce explicit schemes and kind-tagged variable identity.
- Implement instantiation, environment-aware generalization, and skolemized
  annotation checking.
- Complete traversal/occurs checks across every row, mapper, and nested type.
- Add constraint origins and structured diagnostic categories.
- Eliminate `Any` as a successful solution for field requirements and overloads;
  recoverable error nodes must not reach successful query emission.

Exit: row-requirement union, same-field conflicts, closed-row failures, cyclic
types, let polymorphism, and monomorphic lambda parameters behave correctly.

### M2. Add expression types, phases, and structural mapping

- Add `Kind::Phase`, expression types, phase terms, and the phase solver.
- Implement `ExprOf(phase, inputRow)` in value mappers and row normalization.
- Add key-list indices and extend `KeyMap` with `KeepOnly`, `DropKeys`, and
  `RenameMap`; reuse `RowExpr::MapKey` for all three. Complete kind checking,
  substitution, and deferred `ValidKeyMap` constraints over their payloads.
- Extend key mapping with keep/drop decisions, row-level ordering, and checked
  normalization. Close finite picks over open inputs, preserve omission tails,
  and defer unknown mapper indices without discarding source requirements.
- Implement constraints for SQL values, ordering, and grouping.
- Add expression/row-tail type parsing and stable diagnostic printing.
- Test row/window argument-order independence and aggregate incompatibility.
- Support mapping inference through named and open projection rows.

Exit: the type representation can express all signatures in this plan without
new aggregate or projection type constructors.

### M3. Parse first-class fields and remove implicit callback syntax

Primary files: `parser/lexer.rs`, `parser/grammar.rs`, `ast.rs`, `modules.rs`.

- Add `.field`, `.<field`, and `.>field` nodes with source spans.
- Remove implicit-lambda scanning/wrapping and special `that` handling.
- Make record/list arguments ordinary values in all call spellings.
- Fix postfix access precedence and preserve the documented adjacency rule.
- Update name resolution and substitution for selectors and added type forms.
- Retain ordinary lambdas, operator sections, curried calls, `&`, and `$`.

Exit: adding redundant parentheses or binding an expression to a name does not
change its meaning or function arity. Parser tests assert source AST structure,
not generated callback parameters.

### M4. Elaborate expressions, lifts, and overload choices

- Introduce typed expression nodes and the checked-program API.
- Implement bidirectional checking of calls, annotations, records, and lists.
- Implement `Projectable` constraints and evidence so transformer results can
  be checked structurally without broad implicit coercions of function types.
- Insert scalar lifts and row-to-window promotions as explicit evidence.
- Preserve named constant and constant-only projection behavior.
- Elaborate static selector lists and rename records into indexed witnesses,
  including named configurations; reject runtime-dependent specifications.
- Type `keepOnly` and `dropKeys` as ordinary witness constructors that retain
  their key-list indices through aliases, generic helpers, and module imports.
- Make overload resolution independent of declaration/argument traversal order.
- Keep valid generalized constraints attached to imported/reused helpers.
- Give scalar SQL templates expression signatures and retain ordinary scalar
  operations only with a defined host interpretation/constant evaluator.

Exit: reusable row-polymorphic predicates and scalar helpers infer and elaborate
correctly without parser special cases. No inferred coercion erases row or
phase requirements.

### M5. Switch scalar query operations to checked expressions

Primary files: `core.sagate`, `prelude.sagate`, checker relational rules,
`sagate-sql/src/compiler/entry.rs`, `expressions.rs`, and `relational.rs`.

- Update `where`, `select`, `order`, table, mapper, and limit contracts.
- Implement structural projection checking and schema extraction.
- Implement the checked record `mapKey` primitive and define `pick`, `omit`,
  and `rename` as ordinary library wrappers. Preserve selection order,
  remaining tails, complete field types, and simultaneous renaming.
- Lower query `mapKey` through the same symbolic-column projection path. Apply
  strict missing-field/collision checks to both overloads; replace legacy
  collision overwrites and retain right-biased behavior only for `merge`.
- Add the row-transformer overload of `select`. Apply the typed function to a
  symbolic input-column record, then check and lower its result through the
  same path as an explicit projection record. Support composition and aliases.
- Bind expression field paths at query consumption and lower typed scalar IR.
- Remove callback extraction from scalar query lowering.
- Preserve `ForeignId` dispatch, prelude shadowing, and module alias behavior.
- Enforce nullable predicate handling and supported SQL output value types.

Exit: scalar pipelines with rapidly changing shapes work end to end, and a
field removed or renamed by a preceding stage is rejected at its later use.
`select (pick ...)`, `select (omit ...)`, `select (rename ...)`, and composed or
imported transformers match explicit projections. Static selectors work through
generic witness parameters. Direct `mapKey` and the helper wrappers have the
same schemas, field order, SQL results, and rejection behavior. Unknown retained
schemas and unsupported empty SQL projections have precise errors.

### M6. Implement structured join expressions

- Add left/right selector typing and environment adapters.
- Check each side's constraints against the corresponding input schema.
- Lower source identities to distinct SQL aliases, including self-joins.
- Apply outer-join nullability only to the resulting schema and expressions.
- Preserve the chosen output collision policy explicitly in projection SQL.

Exit: mismatched join keys, missing fields on one side, ambiguous plain field
references, and incorrect outer-join nullability are diagnosed. All four join
modes execute correctly on fixtures.

### M7. Implement phase-typed aggregate expressions

- Replace old `Aggregate`/`Group` result wrappers with phase transitions.
- Split `count` from `countOf`; define nullable aggregate signatures and helpers.
- Represent `count` as a nullary foreign operation producing an expression;
  remove the fake argument/function requirement from its prelude and backend
  path. Keep template arity checks aligned with registered operation metadata.
- Replace `AggregateField` extraction with typed aggregate expression lowering.
- Collect and preserve exact grouping keys through scalar compositions.
- Implement global aggregation cardinality, including constant-only payloads.
- Reject illegal phases and support aggregate-then-filter and two-stage
  aggregation through ordinary relation boundaries.

Exit: computed grouping keys, composed aggregate results, empty inputs,
all-null inputs, and nullable outputs pass type and SQL execution tests.

### M8. Implement windows and frame validation

- Add foreign IDs and signatures for the initial window functions and options.
- Check ordinary specification records and their shared row constraints.
- Implement initial `ROWS` frame constructors and validation.
- Enforce row-only arguments/partition/order keys and function-specific options.
- Lower window calls and preserve projection/filter boundaries under optimization.
- Use the dependency adjustments established in M0 if required.

Exit: top-N-per-partition, running and whole-partition sums, lag/lead, nullable
empty frames, and aggregate-then-window pipelines execute correctly. Same-stage
nested windows and unsupported frame modes are rejected.

### M9. Complete the breaking migration and remove parallel paths

- Migrate all examples, README language documentation, and embedded preludes.
- Rewrite `type-system-design.md` as the implemented specification once the
  replacement is complete; keep this document as the implementation record.
- Replace legacy implicit-lambda tests with expression, phase, and constraint
  tests. Keep explicit-lambda tests for ordinary functional code.
- Remove old field `Any` fallbacks, lambda-specific relation validators,
  aggregate field extraction, and compiler-side type reconstruction.
- Update public exports and module diagnostics to use the checked representation.
- Ensure unsupported legacy syntax fails clearly instead of changing meaning
  through hidden compatibility rewrites.

Exit: one checker/elaboration path and one SQL lowering path implement the new
design, and every documented example uses that path.

## 11. Test and acceptance matrix

Tests should establish semantic properties and rejection boundaries, not merely
mirror constructors. Add focused checker tests instead of continuing to place
all inference tests in `parser/tests.rs`.

| Area | Required positive cases | Required negative cases |
| --- | --- | --- |
| Rows | Accumulated disjoint fields; permutations; reused open predicates | Same-field type conflicts; missing closed fields; cyclic tails |
| Schemes | Independent uses across schemas/modules; generic scalar helpers | Generalizing captured variables; polymorphic lambda parameters; false annotations |
| Constants | Literals, named values, constant records, all three phases | Lifting queries/functions; unwrapping expressions; erasing a row dependency |
| Parser | Parentheses invariance; ordinary records/lists; selector adjacency | Old `that` magic; accidental callback wrapping; malformed selectors |
| Phases | Row/window mixtures in both operand orders; constant polymorphism | Row/aggregate mixtures; aggregate/window mixtures; downward promotion |
| Projection | Named projections; constants mixed with fields; changing schemas | Projecting compiler option values; using a removed or renamed field |
| Selection transformers | `select (pick ...)`, omit, rename, composition, imported helpers, mixed expression/constant results | Scalar/non-record results; illegal returned phases; ambiguous overloads |
| Key-mapper reuse | Helpers equivalent to direct `mapKey`; query/record agreement; imported mapper witnesses; nested mappings | Lost selector indices; discarded intermediate validity constraints; silent collision overwrites |
| Pick/omit | Static named selectors; retained value types; closed pick output; open omit tails; records of expressions | Missing or duplicate selectors; runtime label lists; accidentally closing an unknown tail |
| Rename | Simultaneous swaps; identity; unchanged field types/order; open-tail constraints | Missing sources; repeated destinations; collisions with unchanged fields; late collisions after instantiation |
| Joins | Distinct schemas; self-joins; reusable side predicates; all modes | Wrong side requirement; incompatible key types; plain ambiguous selector |
| Aggregates | Computed keys; composed results; count/countOf; global constants | Ungrouped fields; nested aggregates; window inputs to aggregates |
| Windows | Partitioning, ordering, running frames, later filtering | Same-stage nesting; window keys in a specification; invalid frames |
| Nullability | Empty aggregates; all-null input; outer joins; explicit `isTrue` | Treating nullable expressions as guaranteed non-null |
| Backend | Correct optimized results and schemas across stage boundaries | Optimizer changes to window rank/filter or aggregate cardinality |
| Extensibility | Aliased/imported custom combinators and operator overrides | Backend dispatch that depends on public spelling |

Add small deterministic property tests or generated cases for row-unification
symmetry, operand-order independence of phase solving, preservation of required
labels after substitution, and fresh scheme instantiation. These target the
soundness risks rather than incidental representation choices.
Also check that pick/omit/rename agree with their pure record semantics and
their direct `mapKey` equivalents, that swaps do not depend on mapping iteration
order, and that valid label operations commute with expression-value mapping
without modifying its input-row index. Assert that picking `["name", "id"]`
from `{ id, name, age }` emits columns in `name, id` order, while omission and
renaming retain relative input order. Check that a later pick cannot hide an
earlier rename collision or missing-field error. Ensure `mapValue` still
preserves field labels and count.
Compare selection through a transformer with the equivalent explicit record,
including output schemas and SQL results. Reuse one transformer on different
compatible input schemas to detect captured aliases or shared inference cells.

For SQL execution fixtures, compare values, nulls, row multiplicity, and order
only when an explicit final order makes it meaningful. Include empty tables,
duplicate partition/group keys, nulls, and self-joins. Assert both pre- and
post-optimization behavior where an optimizer boundary is relevant.

Run milestone-specific tests while developing, followed by the workspace checks
at integration points:

```sh
cargo test -p sagate-core
cargo test -p sagate-sql
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p sagate-cli -- examples/report.sagate
cargo run -p sagate-cli -- --dialect sqlite examples/report.sagate
cargo run -p sagate-cli -- examples/modules/report.sagate
```

Use baseline findings from M0 to distinguish existing failures from regressions.
Do not claim another SQL dialect is covered by SQLite execution alone; add
rendering/AST assertions and explicit backend capability diagnostics.

## 12. Soundness boundary and completion criteria

For the scalar core, use the reference interpretation
`expr row r a` as a typed computation over a row satisfying `r`. `Pure` ignores
the row, fields read only required labels, and scalar applications preserve
their declared operand/result types. Join adapters select one component of a
structured input. Window promotion changes permitted use, not the scalar
meaning of an ordinary column expression.

Aggregation and windows additionally require stage-specific semantics:
grouping-key collection, empty-group rules, frame validation, and preserved
relational boundaries. An `Expr` type alone is not a proof of those compiler
properties. Trusted foreign signatures and declared database schemas remain
explicit assumptions; SQL arithmetic exceptions are outside a claim that
column references and expression types are valid.

The redesign is complete when:

1. All documented forms type-check and compile using first-class expressions.
2. Row, phase, overload, and mapping requirements survive named definitions,
   ordinary higher-order application, and module imports.
3. Constants lift without making row-dependent values appear constant.
4. Aggregation and windows reject invalid stage combinations before SQL output.
5. The SQL compiler consumes checker evidence instead of rediscovering types.
6. Execution tests demonstrate correct grouping, nullability, joins, and
   window/filter placement after optimization.
7. The parser has no marker-scanning implicit-lambda machinery.
8. The legacy type/compilation paths and compatibility assumptions are removed.
9. Pick/omit/rename preserve exact field types and required tail constraints,
   and work as reusable ordinary functions passed to `select`. All three use
   the checked `mapKey` primitive and the existing `mapkey` row constructor,
   without introducing a new projection type or parser special case.

A future extension can add more window functions, frames, correlated query
forms, or scalar types. None should bypass these invariants through `Any`,
discarded constraints, or broad implicit phase conversions.
