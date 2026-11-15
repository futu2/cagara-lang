# Research records

These are **dated records, not documentation**. Each was written while deciding what
Cagara should become, and each is kept unedited — including the parts that turned out to
be wrong, because a survey that has been quietly corrected is no longer evidence of what
was known at the time.

| record | what it is |
|---|---|
| [`CORE-CALCULUS.md`](CORE-CALCULUS.md) | the core calculus: semantics layers, row types, phases, `maybe`, the stage boundary |
| [`QUERY-ALGEBRA.md`](QUERY-ALGEBRA.md) | candidate relational algebras, and the orthogonal decomposition that was chosen |
| [`SQL-LOGIC-RESEARCH.md`](SQL-LOGIC-RESEARCH.md) | SQL's logic across seven layers, from relational calculus to U-semiring and structural recursion |
| [`TYPE-SYSTEM-RESEARCH.md`](TYPE-SYSTEM-RESEARCH.md) | `ReaderT rowShape`, sugar-free expressions, and what stays checkable |
| [`PURE-FP-DSL.md`](PURE-FP-DSL.md) | the argument for a total, lawful, deep-embedded Haskell-style DSL |
| [`EXPRESSING-SQL.md`](EXPRESSING-SQL.md) | the same theory expressed as writable code, construct by construct |
| [`DESIGN-REVIEW.md`](DESIGN-REVIEW.md) | a review of the 0.3.2 code, with its findings and proposals |

**Read these for the reasoning, not for the rules.** The documents that track the code
are one level up: [`../LEARN.md`](../LEARN.md) (a guide), [`../CORE.md`](../CORE.md)
(compile-time semantics), [`../NULLABILITY.md`](../NULLABILITY.md),
[`../ROW-TYPES.md`](../ROW-TYPES.md), [`../SQL-DIALECTS.md`](../SQL-DIALECTS.md),
[`../ARCHITECTURE.md`](../ARCHITECTURE.md), [`../ENGINE-TESTS.md`](../ENGINE-TESTS.md),
[`../PERFORMANCE.md`](../PERFORMANCE.md) and [`../PLAN.md`](../PLAN.md) (what is done and
what is left). Where a record and a tracking document disagree, the tracking document is
right.

Their ```` ```cagara ```` examples are still compiled by
`crates/cagara-cli/tests/docs_are_true.rs`, so a record that quotes syntax the language
no longer accepts fails the build like any other document. That is deliberate: the prose
is frozen, but a stale *example* is a defect rather than a record.
