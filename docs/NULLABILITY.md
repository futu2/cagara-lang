# Nullability in Cagara

`maybe a` means a value of type `a` may be SQL `NULL`. A type variable in a
function signature means a non-null type, so `expr r a` does not accept
`expr r (maybe a)` unless the function explicitly asks for `maybe a`.

| Source | Type behavior |
|---|---|
| A column declared `maybe int` | Reads as a nullable expression |
| The possibly absent side of a left, right, or full join | Its output columns become `maybe` |
| `sum`, `avg`, `min`, `max` and their `*Filter` forms | Return `maybe`, including for empty/all-null input |
| `lag`, `lead`, `over` | Return `maybe` |
| `count`, `countOf`, `countDistinct`; `over spec count`, `filtered p count` | Return non-null `int` |
| `countOfMaybe`, `countDistinctMaybe` | Return non-null `int` |

The filtered aggregate helpers take a non-null `bool` predicate over the
aggregate's input rows. Their result type is the same as the unfiltered
aggregate, so a filtered `sum`/`avg`/`min`/`max` is `maybe` and a filtered
`count`/`countOf`/`countDistinct` is non-null `int`.

A nullable column reaches an aggregate under the *same* name: `sum`, `avg`,
`min` and `max` accept `maybe` and pass it through unchanged, which is how a
nullable column keeps its meaning. SQL already skips `NULL` in `SUM`/`AVG`/
`MIN`/`MAX`/`COUNT(x)`, so `sum .amount` emits `SUM(amount)`. Coalescing first
is a different query: over `[NULL, 5]`, `avg .amount` is `5` but
`avg (coalesce 0 .amount)` is `2.5`.

Use these helpers to handle nullable values:

- `coalesce default value` and `value ?? default` produce a non-null value.
- `nothing` is SQL `NULL` at any nullable type.
- `just value` marks a non-null value as nullable; it does not create `NULL`.
- `nullIf value other` produces `NULL` when two non-null values are equal.
- `isNull value` and `isNotNull value` test nullability and return non-null `bool`.
- `isTrue value` treats a nullable boolean `NULL` as false.
- `eqMaybe left right` compares nullable values and treats two `NULL`s as equal.

Operators require non-null expressions. A `where` takes a `maybe bool`
directly — its `NULL` condition drops the row — and a nullable value elsewhere
is handled with `coalesce` when a missing value should have a default. After a
left join, for example, `sum .amount` keeps SQL's `SUM(amount)` meaning, while
`sum (coalesce 0.0 .amount)` treats a missing amount as zero:

```haskell
total = coalesce 0.0 (sum .amount)
```

`countOf` follows the same non-null argument rule, even though SQL `COUNT(x)`
ignores nulls. `countOfMaybe` is the explicit spelling and counts matched rows
on a nullable join side:

```haskell
matches = countOfMaybe .right_id
```

The synthetic row created for an unmatched left-side row has a `NULL` right key,
so it is not counted. The equivalent without the helper is an indicator sum:
`coalesce 0 (sum (ifThenElse (isNotNull .right_id) 1 0))`.
These explicit rules make null handling visible in the query; see the report
example for a complete left-join aggregation.
