# Nullability in Cagara

`maybe a` means a value of type `a` may be SQL `NULL`. A type variable in a
function signature means a non-null type, so `expr r a` does not accept
`expr r (maybe a)` unless the function explicitly asks for `maybe a`.

| Source | Type behavior |
|---|---|
| A column declared `maybe int` | Reads as a nullable expression |
| The possibly absent side of a left, right, or full join | Its output columns become `maybe` |
| `sum`, `avg`, `min`, `max` and their `*Filter` forms | Return `maybe`, including for empty/all-null input |
| `sumMaybe`, `avgMaybe`, `minMaybe`, `maxMaybe` | Return `maybe`, including for empty/all-null input |
| `lag`, `lead`, `sumOver`, `avgOver` | Return `maybe` |
| `count`, `countOf`, `countDistinct`, `countOver`; `countFilter`, `countOfFilter`, `countDistinctFilter` | Return non-null `int` |
| `countOfMaybe`, `countDistinctMaybe` | Return non-null `int` |

The filtered aggregate helpers take a non-null `bool` predicate over the
aggregate's input rows. Their result type is the same as the unfiltered
aggregate, so `sumFilter`/`avgFilter`/`minFilter`/`maxFilter` are `maybe` and
`countFilter`/`countOfFilter`/`countDistinctFilter` are non-null `int`.

The `*Maybe` aggregates are how a nullable column reaches an aggregate without
changing its meaning. SQL already skips `NULL` in `SUM`/`AVG`/`MIN`/`MAX`/
`COUNT(x)`, so `sumMaybe .amount` emits `SUM(amount)`. Coalescing first is a
different query: over `[NULL, 5]`, `avgMaybe` is `5` but
`avg (coalesce 0 .amount)` is `2.5`.

Use these helpers to handle nullable values:

- `coalesce default value` and `value ?? default` produce a non-null value.
- `nothing` is SQL `NULL` at any nullable type.
- `just value` marks a non-null value as nullable; it does not create `NULL`.
- `nullIf value other` produces `NULL` when two non-null values are equal.
- `isNull value` and `isNotNull value` test nullability and return non-null `bool`.
- `isTrue value` treats a nullable boolean `NULL` as false.
- `whereTrue predicate` applies that `NULL`-as-false rule while filtering.
- `eqMaybe left right` compares nullable values and treats two `NULL`s as equal.

Operators require non-null expressions, and the ordinary aggregate signatures
do too. Use a `*Maybe` form for an aggregate input; handle a nullable value with
`coalesce` when a missing value should have a default. After a left join, for
example, `sumMaybe .amount` keeps SQL's `SUM(amount)` meaning, while
`sum (coalesce 0.0 .amount)` treats a missing amount as zero:

```haskell
total = coalesce 0.0 (sumMaybe .amount)
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
