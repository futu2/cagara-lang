//! Dialect rewriting for the whole statement.
//!
//! The IR and `sql` templates are ANSI. sqlglot's `dialects::transform` only
//! rewrites the outermost SELECT's columns / WHERE / GROUP BY / HAVING, so
//! this walks every block (derived tables, CTE bodies, set-operation
//! branches, join inputs) and every expression slot (including ORDER BY and
//! join ON), applying sqlglot's expression rewrites to each. String
//! concatenation is lowered here: `||` means OR in MySQL, so the MySQL and
//! T-SQL families get `CONCAT(a, b, ...)`. Cagara's date and string
//! intrinsics (`CAGARA_*`) are lowered here too, for every dialect including
//! ANSI.

use crate::stage::transform_deep;
use sqlglot_rust::ast::{
    BinaryOperator, Expr, OrderByItem, QuoteStyle, SelectItem, SelectStatement, TableSource,
    TypedFunction, UnaryOperator,
};
use sqlglot_rust::{Dialect, Statement};

/// The dialect grouping shared by statement rewriting and intrinsic lowering.
///
/// Both phases need "which engine family is this", and they must agree: the
/// MySQL family gets `CONCAT` *and* the MySQL intrinsic spellings. One map here
/// is the single place a new dialect is classified.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fam {
    Ansi,
    Mysql,
    Sqlite,
    Duck,
    Tsql,
    BigQuery,
    Snowflake,
    Trino,
    Spark,
}

/// Does this dialect have the aggregate `FILTER (WHERE …)` clause?
///
/// Standard SQL since 2003, and every engine here that has it treats it as a
/// suffix of the aggregate call. The ones that do not get the portable spelling
/// instead, applied by [`rewrite`] — `SUM(CASE WHEN p THEN x END)`.
/// Does this target have a `QUALIFY` clause — a filter that runs after the window
/// functions, so it can read their output in the same `SELECT`?
///
/// Four do. Everywhere else the filter becomes a derived table, which is what
/// `lower.rs` does when this is false: the clause is not rewritten per dialect by
/// sqlglot (it passes `QUALIFY` through to every target, which would be a syntax
/// error on the rest), so the decision has to be made before the statement exists.
pub(crate) fn supports_qualify(d: Dialect) -> bool {
    matches!(
        d,
        Dialect::DuckDb | Dialect::Snowflake | Dialect::BigQuery | Dialect::Teradata
    )
}

pub(crate) fn supports_filter(d: Dialect) -> bool {
    !matches!(fam(d), Fam::Mysql | Fam::Tsql | Fam::BigQuery | Fam::Spark)
}

pub(crate) fn fam(d: Dialect) -> Fam {
    match d {
        Dialect::Mysql | Dialect::Doris | Dialect::SingleStore | Dialect::StarRocks => Fam::Mysql,
        Dialect::Sqlite => Fam::Sqlite,
        Dialect::DuckDb => Fam::Duck,
        Dialect::Tsql | Dialect::Fabric => Fam::Tsql,
        Dialect::BigQuery => Fam::BigQuery,
        Dialect::Snowflake => Fam::Snowflake,
        Dialect::Trino | Dialect::Presto | Dialect::Athena => Fam::Trino,
        Dialect::Spark | Dialect::Databricks => Fam::Spark,
        _ => Fam::Ansi,
    }
}

pub fn rewrite(stmt: Statement, to: Dialect) -> Result<Statement, String> {
    let Statement::Select(mut sel) = stmt else {
        return Ok(stmt);
    };
    block(&mut sel, to)?;
    Ok(Statement::Select(sel))
}

/// Whether the target engine has a `FULL [OUTER] JOIN` spelling.
///
/// MySQL has only `LEFT`/`RIGHT` joins; emitting `FULL JOIN` there produces
/// SQL the engine rejects. The Cagara query stays expressible as a union of the
/// two outer joins, but rewriting it here would have to know which predicate
/// null-extends which side, so the backend refuses it instead.
pub(crate) fn supports_full_join(d: Dialect) -> bool {
    fam(d) != Fam::Mysql
}

/// Is this the Trino grammar family (`OFFSET` before `LIMIT`)?
fn trino(d: Dialect) -> bool {
    matches!(d, Dialect::Trino | Dialect::Presto | Dialect::Athena)
}

fn tsql(d: Dialect) -> bool {
    fam(d) == Fam::Tsql
}

fn mysql(d: Dialect) -> bool {
    fam(d) == Fam::Mysql
}

fn concat_as_function(d: Dialect) -> bool {
    matches!(fam(d), Fam::Mysql | Fam::Tsql)
}

/// Dialects whose string literals treat `\` as an escape character, so a
/// literal `\'` would end the string early.
///
/// This cuts across [`Fam`] (Hive has no family of its own), so it is its own
/// list rather than a family test.
fn backslash_escapes(d: Dialect) -> bool {
    mysql(d)
        || matches!(
            d,
            Dialect::BigQuery
                | Dialect::Snowflake
                | Dialect::Hive
                | Dialect::Spark
                | Dialect::Databricks
                | Dialect::ClickHouse
        )
}

fn block(sel: &mut SelectStatement, to: Dialect) -> Result<(), String> {
    // Inner blocks first: CTE bodies, then the FROM / JOIN inputs.
    for cte in &mut sel.ctes {
        nested(&mut cte.query, to)?;
    }
    if let Some(from) = &mut sel.from {
        source(&mut from.source, to)?;
    }
    for j in &mut sel.joins {
        source(&mut j.table, to)?;
    }

    // `offset` alone: a lone OFFSET is not valid everywhere, so spell the
    // "no limit" the dialect needs.
    //
    //   * MySQL has no OFFSET of its own; its documented idiom is the largest
    //     BIGINT UNSIGNED as the LIMIT.
    //   * SQLite rejects a bare `OFFSET` outright, and *also* rejects the
    //     MySQL trick above (`datatype mismatch`): its spelling is `LIMIT -1`.
    //   * T-SQL rejects `OFFSET n ROWS` with no FETCH — the two are one
    //     syntactic unit — and additionally requires an ORDER BY. Setting the
    //     limit here is what makes sqlglot emit
    //     `OFFSET n ROWS FETCH NEXT m ROWS ONLY`.
    if sel.offset.is_some() && sel.limit.is_none() {
        if mysql(to) {
            sel.limit = Some(Expr::Number("18446744073709551615".into()));
        } else if matches!(to, Dialect::Sqlite) {
            sel.limit = Some(Expr::Number("-1".into()));
        } else if tsql(to) {
            if sel.order_by.is_empty() {
                return Err("T-SQL needs an `order` before `offset`".into());
            }
            // `FETCH NEXT` needs a row count; mimic sqlglot's own choice for
            // "all remaining rows" on this dialect.
            sel.limit = Some(Expr::Number("9223372036854775807".into()));
        }
    }

    // Every expression slot of this block.
    for item in &mut sel.columns {
        if let SelectItem::Expr { expr: e, .. } = item {
            *e = expr(std::mem::replace(e, Expr::Null), to)?;
            // T-SQL has no boolean values, only conditions: a condition as a
            // column becomes 1 / 0, and stays NULL when it is unknown.
            if tsql(to) && is_condition(e) {
                let p = std::mem::replace(e, Expr::Null);
                *e = Expr::Case {
                    operand: None,
                    when_clauses: vec![
                        (p.clone(), Expr::Number("1".into())),
                        (not(p), Expr::Number("0".into())),
                    ],
                    else_clause: None,
                };
            }
        }
    }
    nulls_last(&mut sel.order_by, to);
    for e in [&mut sel.where_clause, &mut sel.having]
        .into_iter()
        .flatten()
    {
        *e = expr(std::mem::replace(e, Expr::Null), to)?;
    }
    for e in &mut sel.group_by {
        *e = expr(std::mem::replace(e, Expr::Null), to)?;
    }
    for o in &mut sel.order_by {
        o.expr = expr(std::mem::replace(&mut o.expr, Expr::Null), to)?;
    }
    for j in &mut sel.joins {
        if let Some(on) = &mut j.on {
            *on = expr(std::mem::replace(on, Expr::Null), to)?;
        }
    }

    // Statement-level rewrites (LIMIT → TOP / FETCH, quoting) for this block.
    // Expressions are already rewritten; sqlglot's pass over them is a no-op.
    if to == Dialect::Ansi {
        return Ok(());
    }
    let Statement::Select(s) = sqlglot_rust::dialects::transform(
        &Statement::Select(std::mem::replace(sel, empty())),
        Dialect::Ansi,
        to,
    ) else {
        // sqlglot's transform is supposed to pass a SELECT back through; a
        // future version that does not must be an error, not an empty block.
        return Err("internal: dialect transform dropped a SELECT block".into());
    };
    *sel = s;
    // Trino puts `OFFSET` *before* `LIMIT` — its grammar is `[ OFFSET n [ ROW |
    // ROWS ] ] [ LIMIT n ]`, in that order — and sqlglot writes `LIMIT` then
    // `OFFSET` for every target. `LIMIT 2 OFFSET 1` is a parse error there
    // (`mismatched input 'OFFSET'`), so the pair is spelled with the standard
    // clause instead: `OFFSET 1 FETCH FIRST 2 ROWS ONLY`, which the same
    // generator writes in the right order and Trino reads. Only the pair needs
    // it; a limit alone is `LIMIT n` and an offset alone is `OFFSET n`.
    //
    // After the transform, not before: the transform is what rewrites the
    // statement into the target's own shape, and it turns `fetch_first` back
    // into a `LIMIT` on the way in.
    if trino(to) && sel.limit.is_some() && sel.offset.is_some() {
        sel.fetch_first = sel.limit.take();
    }
    Ok(())
}

/// The last thing done to a statement, after it is generated.
///
/// One fixup lives here, and it is here rather than in the AST because the AST
/// cannot say it: sqlglot's `SetOperationStatement` has a single `all: bool`, so the
/// generator writes `UNION` or `UNION ALL` and nothing else. BigQuery's grammar is
/// `UNION { ALL | DISTINCT }` — plain `UNION` is a *syntax error*, not a default —
/// and the same for `INTERSECT` and `EXCEPT`. Python sqlglot encodes exactly this
/// (`SET_OP_DISTINCT_BY_DEFAULT` is `None` for all three on BigQuery) and writes the
/// keyword out; the Rust crate has no equivalent hook, so the keyword is added to the
/// text.
///
/// The scan is quote-aware, so a `'UNION'` in a string literal or a `"union"` column
/// name is left alone.
pub(crate) fn finalize(sql: String, to: Dialect) -> String {
    if !matches!(to, Dialect::BigQuery) {
        return sql;
    }
    backslash_escaped_literals(&explicit_set_operation_distinct(&sql))
}

/// Write a quote inside a string literal the way BigQuery writes it.
///
/// sqlglot's generator doubles a quote (`''`) for every dialect, which is the
/// standard-SQL spelling and what PostgreSQL, DuckDB, SQLite, MySQL, Trino and T-SQL
/// read. BigQuery does not: it has C-style escapes, and two adjacent string literals
/// are an *error*, not a concatenation —
///
/// ```text
/// ParseError: Adjacent string literals need to be separated by whitespace or comments.
/// ```
///
/// — so a value holding a quote produces SQL BigQuery cannot parse. Only the quote
/// needs rewriting: the crate's dialect transform already escapes backslashes for the
/// targets that read them as escapes, which is why `'a\\b'` is right there and
/// `'it''s'` is not.
///
/// Snowflake and Spark accept the doubled quote too, so they are left alone: this
/// changes what is broken and nothing else.
fn backslash_escaped_literals(sql: &str) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(sql.len() + 8);
    let mut i = 0;
    let mut in_literal = false;
    while i < chars.len() {
        let c = chars[i];
        if c != '\'' {
            out.push(c);
            i += 1;
            continue;
        }
        if !in_literal {
            in_literal = true;
            out.push(c);
            i += 1;
            continue;
        }
        // Inside a literal, `''` is one quote and BigQuery spells it `\'`; a lone
        // quote closes the literal.
        if chars.get(i + 1) == Some(&'\'') {
            out.push_str("\\'");
            i += 2;
            continue;
        }
        in_literal = false;
        out.push(c);
        i += 1;
    }
    out
}

/// Add `DISTINCT` to a set operation that has neither `ALL` nor `DISTINCT`.
fn explicit_set_operation_distinct(sql: &str) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(sql.len() + 32);
    let mut i = 0;
    // A quote opens a run that no keyword is looked for in; `''` inside a string is
    // an escaped quote rather than the end of it.
    let mut quote: Option<char> = None;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            out.push(c);
            if c == q {
                if chars.get(i + 1) == Some(&q) {
                    out.push(q);
                    i += 1;
                } else {
                    quote = None;
                }
            }
            i += 1;
            continue;
        }
        if c == '\'' || c == '"' || c == '`' {
            quote = Some(c);
            out.push(c);
            i += 1;
            continue;
        }
        // A word: read it whole, then decide.
        if c.is_alphanumeric() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let upper = word.to_ascii_uppercase();
            out.push_str(&word);
            if matches!(upper.as_str(), "UNION" | "INTERSECT" | "EXCEPT") {
                let rest: String = chars[i..].iter().collect();
                let next = rest.trim_start().to_ascii_uppercase();
                if !(next.starts_with("ALL") || next.starts_with("DISTINCT")) {
                    out.push_str(" DISTINCT");
                }
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// A statement nested in a query: a derived table, a CTE body, or a branch of
/// a set operation. A set operation is not a `Select`, so its own ORDER BY /
/// LIMIT / OFFSET are rewritten here; everything else is reached by `block`.
fn nested(s: &mut Statement, to: Dialect) -> Result<(), String> {
    match s {
        Statement::Select(sel) => block(sel, to),
        Statement::SetOperation(set) => {
            nested(&mut set.left, to)?;
            nested(&mut set.right, to)?;
            nulls_last(&mut set.order_by, to);
            for o in &mut set.order_by {
                o.expr = expr(std::mem::replace(&mut o.expr, Expr::Null), to)?;
            }
            for e in [&mut set.limit, &mut set.offset].into_iter().flatten() {
                *e = expr(std::mem::replace(e, Expr::Null), to)?;
            }
            Ok(())
        }
        // Cagara builds only SELECT and set-operation trees.
        _ => Ok(()),
    }
}

/// Rewrite the statements nested inside an expression, however deep the
/// expression puts them.
///
/// sqlglot's AST has three expression forms that carry a `Statement`, and
/// `Expr::transform` does not descend into any of them — it walks expressions,
/// and a statement is not one. So each is rewritten here, by `nested`, which is
/// what every other block already goes through.
fn subqueries(e: Expr, to: Dialect) -> Result<Expr, String> {
    crate::stage::try_transform_deep(e, &|e| {
        Ok(match e {
            Expr::Exists { subquery, negated } => Expr::Exists {
                subquery: Box::new(beside(*subquery, to)?),
                negated,
            },
            Expr::Subquery(sub) => Expr::Subquery(Box::new(beside(*sub, to)?)),
            Expr::InSubquery {
                expr,
                subquery,
                negated,
            } => Expr::InSubquery {
                expr,
                subquery: Box::new(beside(*subquery, to)?),
                negated,
            },
            other => other,
        })
    })
}

/// A statement that lives inside an expression, rewritten in place.
fn beside(mut s: Statement, to: Dialect) -> Result<Statement, String> {
    nested(&mut s, to)?;
    Ok(s)
}

fn source(src: &mut TableSource, to: Dialect) -> Result<(), String> {
    match src {
        TableSource::Subquery { query, .. } => nested(query, to)?,
        // sqlglot writes a table's schema as is, so quote it here.
        TableSource::Table(t) => {
            if let Some(s) = t.schema.as_mut().filter(|s| crate::stage::needs_quotes(s)) {
                *s = quote(s, to);
            }
        }
        _ => {}
    }
    Ok(())
}

/// An identifier in the dialect's quotes, the quote character doubled.
fn quote(name: &str, to: Dialect) -> String {
    let (open, close) = match QuoteStyle::for_dialect(to) {
        QuoteStyle::Backtick => ('`', '`'),
        QuoteStyle::Bracket => ('[', ']'),
        _ => ('"', '"'),
    };
    let escaped = name.replace(close, &format!("{close}{close}"));
    format!("{open}{escaped}{close}")
}

/// Spell a string↔datetime conversion the way the target does.
///
/// sqlglot emits `TO_CHAR(x, fmt)` / `TO_TIMESTAMP(x, fmt)` for a format
/// conversion, which only PostgreSQL reads. DuckDB and Trino have their own
/// pairs, and both take the same C-style format the template was written with:
/// `STRFTIME`/`STRPTIME` and `DATE_FORMAT`/`DATE_PARSE`.
fn format_call(e: Expr, f: Fam) -> Expr {
    let Expr::TypedFunction { func, filter, over } = e else {
        return e;
    };
    let (name, args) = match (&func, f) {
        (TypedFunction::StrToTime { expr, format }, Fam::Duck) => {
            ("strptime", vec![(**expr).clone(), (**format).clone()])
        }
        (TypedFunction::StrToTime { expr, format }, Fam::Trino) => {
            ("date_parse", vec![(**expr).clone(), (**format).clone()])
        }
        (TypedFunction::TimeToStr { expr, format }, Fam::Duck) => {
            ("strftime", vec![(**expr).clone(), (**format).clone()])
        }
        (TypedFunction::TimeToStr { expr, format }, Fam::Trino) => {
            ("date_format", vec![(**expr).clone(), (**format).clone()])
        }
        _ => {
            return Expr::TypedFunction { func, filter, over };
        }
    };
    Expr::Function {
        name: name.to_string(),
        args,
        distinct: false,
        filter,
        over,
        order_by: Vec::new(),
        within_group: false,
    }
}

/// Rewrite `FILTER (WHERE p)` into the portable `CASE` spelling.
///
/// A filtered aggregate keeps the rows the predicate keeps. Written as `CASE`
/// inside the argument that is the same aggregate for every engine, at the cost
/// of repeating the predicate once per argument — and a filter on `COUNT(*)`,
/// which has no argument, counts a constant instead.
fn fold_filter(e: Expr) -> Expr {
    let case = |p: Expr, then: Expr| Expr::Case {
        operand: None,
        when_clauses: vec![(p, then)],
        else_clause: None,
    };
    match e {
        // A bare call: the filter wraps each argument.
        Expr::Function {
            name,
            args,
            distinct,
            filter: Some(p),
            over,
            order_by,
            within_group,
        } => {
            let p = *p;
            let args = if args.is_empty() {
                vec![case(p, Expr::Number("1".into()))]
            } else {
                args.into_iter().map(|a| case(p.clone(), a)).collect()
            };
            Expr::Function {
                name,
                args,
                distinct,
                filter: None,
                over,
                order_by,
                within_group,
            }
        }
        // A call the parser knows: its argument is its only child, and
        // `COUNT(*)`'s wildcard becomes the constant the predicate counts.
        Expr::TypedFunction {
            func,
            filter: Some(p),
            over,
        } => {
            let p = *p;
            let wrapped = func.transform_children(&|child: Expr| {
                let then = match child {
                    Expr::Wildcard => Expr::Number("1".into()),
                    other => other,
                };
                case(p.clone(), then)
            });
            Expr::TypedFunction {
                func: wrapped,
                filter: None,
                over,
            }
        }
        other => other,
    }
}

/// One expression: intrinsics, concat lowering, then sqlglot's
/// per-expression rewrites (reached by wrapping it in a one-column SELECT).
fn expr(e: Expr, to: Dialect) -> Result<Expr, String> {
    let e = crate::intrinsics::lower(e, to)?;
    // A subquery inside an expression — `EXISTS (…)` from a semi/anti join, and
    // the two other spellings sqlglot's AST has — is a block of its own, and its
    // clauses need the same rewriting as any other. Missing this left a
    // `NULLS LAST` (and, without the statement transform's reach, other ANSI
    // spellings) inside an `EXISTS`, which MySQL and T-SQL cannot read:
    //
    //     WHERE EXISTS (SELECT … FROM (SELECT … ORDER BY id NULLS LAST) AS t1)
    let e = subqueries(e, to)?;
    // A filtered aggregate is `FILTER (WHERE …)` where the target has the
    // clause, and the portable `SUM(CASE WHEN … THEN x END)` where it does not.
    let e = if supports_filter(to) {
        e
    } else {
        transform_deep(e, &fold_filter)
    };
    if to == Dialect::Ansi {
        return Ok(e);
    }
    let e = if backslash_escapes(to) {
        transform_deep(e, &|e| match e {
            Expr::StringLiteral(s) => Expr::StringLiteral(s.replace('\\', "\\\\")),
            other => other,
        })
    } else {
        e
    };
    let e = if concat_as_function(to) {
        transform_deep(e, &concat)
    } else {
        e
    };
    // PostgreSQL has no `TRY_CAST`: `TRY_CAST(x AS DATE)` is a syntax error
    // there, not a NULL, and the server only finds out. The generic `ansi`
    // target keeps the spelling (it is the loose one); a named target that
    // rejects it says so at compile time.
    if to == Dialect::Postgres {
        let found = std::cell::Cell::new(false);
        let e = transform_deep(e, &|e| {
            if matches!(e, Expr::TryCast { .. }) {
                found.set(true);
            }
            e
        });
        if found.get() {
            return Err(
                "PostgreSQL has no `TRY_CAST`, so a `try*` cast cannot be spelled there — the \
                 server would reject the SQL rather than yield NULL. Write the guard yourself \
                 (`CASE WHEN pg_input_is_valid(x, 'date') THEN CAST(x AS DATE) END`, PostgreSQL \
                 16+), or use a `to*` cast when the input is known to be shaped"
                    .to_string(),
            );
        }
        return Ok(e);
    }
    // `TO_CHAR`/`TO_TIMESTAMP` are what sqlglot emits for a format conversion,
    // but only PostgreSQL has them spelled that way among the targets here.
    // DuckDB and Trino say `STRFTIME`/`STRPTIME` and `DATE_FORMAT`/`DATE_PARSE`,
    // with the same C-style format string the template already gave.
    let e = match fam(to) {
        Fam::Duck | Fam::Trino => transform_deep(e, &|e| format_call(e, fam(to))),
        _ => e,
    };
    // Window specs sort NULLs last too.
    let e = transform_deep(e, &|e| match e {
        Expr::Function {
            name,
            args,
            distinct,
            filter,
            over: Some(mut spec),
            order_by,
            within_group,
        } => {
            nulls_last(&mut spec.order_by, to);
            Expr::Function {
                name,
                args,
                distinct,
                filter,
                over: Some(spec),
                order_by,
                within_group,
            }
        }
        Expr::TypedFunction {
            func,
            filter,
            over: Some(mut spec),
        } => {
            nulls_last(&mut spec.order_by, to);
            Expr::TypedFunction {
                func,
                filter,
                over: Some(spec),
            }
        }
        other => other,
    });
    let mut sel = empty();
    sel.columns = vec![SelectItem::Expr {
        expr: e.clone(),
        alias: None,
        alias_quote_style: Default::default(),
    }];
    // The same policy as `block`: sqlglot's transform is supposed to hand the
    // SELECT back. Returning the untransformed expression instead would emit
    // ANSI syntax for a dialect where it means something else (`||` is OR in
    // MySQL), which is worse than failing.
    match sqlglot_rust::dialects::transform(&Statement::Select(sel), Dialect::Ansi, to) {
        Statement::Select(s) => match s.columns.into_iter().next() {
            Some(SelectItem::Expr { expr, .. }) => Ok(expr),
            _ => Err("internal: dialect transform dropped an expression".into()),
        },
        _ => Err("internal: dialect transform dropped a SELECT block".into()),
    }
}

/// Where a dialect sorts NULLs by default: `(last when ascending, last when
/// descending, has NULLS FIRST / LAST)`, or `None` when it is not known
/// (ANSI leaves it to the implementation).
fn null_order(d: Dialect) -> Option<(bool, bool, bool)> {
    use Dialect::*;
    Some(match d {
        DuckDb | Trino | Presto | Athena => (true, true, true),
        Postgres | Oracle | Snowflake | Redshift | Materialize | RisingWave => (true, false, true),
        Sqlite | BigQuery | Spark | Databricks | Hive => (false, true, true),
        _ if mysql(d) || tsql(d) => (false, true, false),
        _ => return None,
    })
}

/// Sort keys marked NULLS LAST, for the target: the clause is dropped where
/// it is the default, and emulated with a leading `CASE WHEN x IS NULL`
/// key where the dialect has no such clause.
fn nulls_last(items: &mut Vec<OrderByItem>, to: Dialect) {
    let Some((asc_last, desc_last, syntax)) = null_order(to) else {
        return;
    };
    let mut out = Vec::with_capacity(items.len());
    for mut it in std::mem::take(items) {
        if it.nulls_first != Some(false) {
            out.push(it);
            continue;
        }
        let default_last = if it.ascending { asc_last } else { desc_last };
        if default_last {
            it.nulls_first = None;
        } else if !syntax {
            let is_null = Expr::IsNull {
                expr: Box::new(it.expr.clone()),
                negated: false,
            };
            out.push(OrderByItem {
                expr: Expr::Case {
                    operand: None,
                    when_clauses: vec![(is_null, Expr::Number("1".into()))],
                    else_clause: Some(Box::new(Expr::Number("0".into()))),
                },
                ascending: true,
                nulls_first: None,
            });
            it.nulls_first = None;
        }
        out.push(it);
    }
    *items = out;
}

/// An expression that is a condition rather than a value in T-SQL.
fn is_condition(e: &Expr) -> bool {
    match e {
        Expr::BinaryOp { op, .. } => matches!(
            op,
            BinaryOperator::Eq
                | BinaryOperator::Neq
                | BinaryOperator::Lt
                | BinaryOperator::Gt
                | BinaryOperator::LtEq
                | BinaryOperator::GtEq
                | BinaryOperator::And
                | BinaryOperator::Or
        ),
        Expr::UnaryOp {
            op: UnaryOperator::Not,
            ..
        } => true,
        Expr::IsNull { .. }
        | Expr::IsBool { .. }
        | Expr::Like { .. }
        | Expr::ILike { .. }
        | Expr::InList { .. }
        | Expr::Between { .. }
        | Expr::Exists { .. } => true,
        Expr::Nested(e) => is_condition(e),
        _ => false,
    }
}

fn not(e: Expr) -> Expr {
    Expr::UnaryOp {
        op: UnaryOperator::Not,
        expr: Box::new(Expr::Nested(Box::new(e))),
    }
}

/// `a || b || c` (any nesting, parenthesized or not) → `CONCAT(a, b, c)`.
fn concat(e: Expr) -> Expr {
    match e {
        Expr::BinaryOp {
            op: BinaryOperator::Concat,
            ..
        } => {
            let mut args = Vec::new();
            flatten(e, &mut args);
            Expr::Function {
                name: "CONCAT".into(),
                args,
                distinct: false,
                filter: None,
                over: None,
                order_by: vec![],
                within_group: false,
            }
        }
        other => other,
    }
}

fn flatten(e: Expr, out: &mut Vec<Expr>) {
    match e {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Concat,
            right,
        } => {
            flatten(*left, out);
            flatten(*right, out);
        }
        Expr::Function { name, args, .. } if name == "CONCAT" => out.extend(args),
        // A parenthesized concatenation, `x <> (y <> z)`, arrives as
        // `Nested(Function CONCAT)`: the walk above is bottom-up.
        Expr::Nested(inner) if matches!(*inner, Expr::Function { ref name, .. } if name == "CONCAT") => {
            flatten(*inner, out)
        }
        other => out.push(other),
    }
}

fn empty() -> SelectStatement {
    SelectStatement {
        comments: vec![],
        ctes: vec![],
        distinct: false,
        top: None,
        columns: vec![],
        from: None,
        joins: vec![],
        where_clause: None,
        group_by: vec![],
        having: None,
        order_by: vec![],
        limit: None,
        offset: None,
        fetch_first: None,
        qualify: None,
        window_definitions: vec![],
        query_options: None,
    }
}
