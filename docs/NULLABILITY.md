# Nullability in Cagara

`maybe a` means a value of type `a` may be SQL `NULL`. A type variable in a
function signature means a non-null type, so `expr r a` does not accept
`expr r (maybe a)` unless the function explicitly asks for `maybe a`.

| Source | Type behavior |
|---|---|
| A column declared `maybe int` | Reads as a nullable expression |
| The possibly absent side of a left, right, or full join | Its output columns become `maybe` |
| `sum`, `avg`, `min`, `max` | Return `maybe`, including for empty/all-null input |
| `lag`, `lead`, `sumOver`, `avgOver` | Return `maybe` |
| `count`, `countOf`, `countDistinct`, `countOver` | Return non-null `int` |

Use these helpers to handle nullable values:

- `coalesce default value` and `value ?? default` produce a non-null value.
- `just value` marks a non-null value as nullable.
- `isNull value` and `isNotNull value` test nullability and return non-null `bool`.
- `isTrue value` treats a nullable boolean `NULL` as false.

Operators and aggregate/window inputs require non-null expressions. Handle a
nullable input before passing it to one. For example, after a left join,
`sum (coalesce 0.0 .amount)` uses zero for a missing amount; `coalesce 0.0`
around the aggregate also gives a default if the aggregate itself has no
value:

```haskell
total = coalesce 0.0 (sum (coalesce 0.0 .amount))
```

`countOf` follows the same non-null argument rule, even though SQL `COUNT(x)`
ignores nulls. To count matched rows on a nullable join side, sum a conditional
indicator instead:

```haskell
matches = coalesce 0 (sum (ifThenElse (isNotNull .right_id) 1 0))
```

This counts true matches while excluding the synthetic row created for an
unmatched left-side row. These explicit rules make null handling visible in
the query; see the report example for a complete left-join aggregation.
