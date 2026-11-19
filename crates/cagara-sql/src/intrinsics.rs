//! Cagara date and string intrinsics, lowered per dialect.
//!
//! sqlglot's typed date functions do not transpile reliably (a `DATEADD`
//! comes out invalid for Postgres, `DATE_TRUNC` / `EXTRACT` pass through
//! unchanged to MySQL and SQLite, which lack them), so prelude templates
//! call `CAGARA_*` functions and this module spells each one for the target.
//! Units and kinds are string literals: `CAGARA_ADD('DATE', 'MONTH', x, n)`.
//!
//! | intrinsic                           | meaning                                   |
//! |-------------------------------------|-------------------------------------------|
//! | `CAGARA_ADD(kind, unit, x, n)`      | `x` plus `n` units, same type as `x`      |
//! | `CAGARA_TRUNC(kind, unit, x)`       | start of the unit containing `x` (ISO weeks start Monday) |
//! | `CAGARA_PART(kind, unit, x)`        | integer field of `x` (`DOW`: 0 = Sunday)  |
//! | `CAGARA_DAYS_BETWEEN(a, b)`         | whole days from date `a` to date `b`      |
//! | `CAGARA_NOW()`                      | current timestamp                         |
//! | `CAGARA_TO_DATE(x)` / `CAGARA_TO_TIMESTAMP(x)` / `CAGARA_TO_STRING(x)` | casts |
//! | `CAGARA_TO_INT(x)` / `CAGARA_TO_FLOAT(x)` | width-correct casts (64-bit) |
//! | `CAGARA_TO_DECIMAL(p, s, x)`        | exact `DECIMAL(p, s)` cast             |
//! | `CAGARA_TRUNC_INT(x)`              | truncate toward zero, then 64-bit int  |
//! | `CAGARA_TRY_INT(x)` / `CAGARA_TRY_FLOAT(x)` | casts that yield NULL on failure |
//! | `CAGARA_TRY_DATE(x)` / `CAGARA_TRY_TIMESTAMP(x)` / `CAGARA_TRY_DECIMAL(p, s, x)` | try-casts |
//! | `CAGARA_LEFT(s, n)` / `CAGARA_RIGHT(s, n)` | first / last `n` characters        |
//! | `CAGARA_STRPOS(s, sub)`             | 1-based position of `sub` in `s`, 0 if absent |
//! | `CAGARA_LENGTH(s)`                  | length in characters (trailing spaces count) |
//! | `CAGARA_LIKE(p, s)` / `CAGARA_ILIKE(p, s)` | SQL `LIKE` / case-insensitive `LIKE` |
//! | `CAGARA_IDIV(a, b)` / `CAGARA_MOD(a, b)` | integer division truncated toward zero, and its remainder |
//!
//! `kind` is `DATE` or `TIMESTAMP`. Dialects not named below get the ANSI /
//! Postgres spelling.

use crate::dialect::{fam, support, Cap, Fam};
use crate::stage::atomic;
use sqlglot_rust::ast::{
    BinaryOperator, Cte, DataType, DateTimeField, Expr, FromClause, OrderByItem, QuoteStyle,
    SelectItem, SelectStatement, SetOperationStatement, SetOperationType, Statement, TableRef,
    TableSource, TypedFunction, UnaryOperator,
};
use sqlglot_rust::Dialect;

/// Lower every intrinsic in `e` (bottom-up, so arguments are lowered first).
/// A `CAGARA_*` call that is not a known intrinsic is an error rather than
/// SQL that the target engine would reject.
pub fn lower(e: Expr, to: Dialect) -> Result<Expr, String> {
    crate::stage::try_transform_deep(e, &|e| node(e, to))
}

fn node(e: Expr, to: Dialect) -> Result<Expr, String> {
    let f = fam(to);
    let Expr::Function {
        name,
        args,
        distinct,
        filter,
        over,
        order_by,
        within_group,
    } = e
    else {
        return Ok(e);
    };
    let upper = name.to_ascii_uppercase();
    if !upper.starts_with("CAGARA_") {
        return Ok(Expr::Function {
            name,
            args,
            distinct,
            filter,
            over,
            order_by,
            within_group,
        });
    }
    if upper == "CAGARA_EXACT_AVG" {
        let [value, Expr::Number(s)] = args.as_slice() else {
            return Err("decimal avg requires a value and its scale".into());
        };
        let scale = s
            .parse::<u32>()
            .ok()
            .filter(|s| *s <= 38)
            .ok_or("decimal avg requires a scale from 0 to 38")?;
        let aggregate = |name: &str| Expr::Function {
            name: name.into(),
            args: vec![value.clone()],
            distinct,
            filter: filter.clone(),
            over: over.clone(),
            order_by: vec![],
            within_group: false,
        };
        return exact_decimal_avg(to, scale, aggregate("SUM"), aggregate("COUNT")).map(atomic);
    }
    if let Some(msg) = list_refusal(&upper, to) {
        return Err(msg);
    }
    if let Some(msg) = decimal_refusal(&upper, to) {
        return Err(msg);
    }
    // A format conversion takes a *format string*, which the language writes in
    // one vocabulary and each target spells in its own; that translation is
    // fallible (an unsupported specifier, a format that is not a literal, a
    // target with no parser), so it runs before the infallible table below.
    if matches!(
        upper.as_str(),
        "CAGARA_PARSE_DATE"
            | "CAGARA_PARSE_TIMESTAMP"
            | "CAGARA_FORMAT_DATE"
            | "CAGARA_FORMAT_TIMESTAMP"
    ) {
        return parse_or_format(&upper, &args, f).map(atomic);
    }
    let lowered = if over.is_none() {
        intrinsic(&upper, &args, f)
    } else {
        // An intrinsic is a scalar date/string function; a windowed call is a
        // different mistake from an unknown name or a wrong arity, so it gets
        // its own message rather than "unknown ... with N argument(s)".
        return Err(format!(
            "the SQL intrinsic `{upper}` cannot be used as a window function"
        ));
    };
    match lowered {
        // Non-atomic results are parenthesized so they keep precedence
        // inside whatever template they were substituted into.
        //
        // `distinct` and `filter` are re-attached because `intrinsic` builds a
        // fresh call from the argument vector and has no room to carry them —
        // which is how `filtered p (arrayAgg .x)` used to emit a plain
        // `ARRAY_AGG(x)`: the predicate reached this function attached to the
        // call, and the rebuilt call dropped it. Only an aggregate can carry
        // either, so an intrinsic without one cannot reach here with them set.
        Some(out) => Ok(atomic(with_aggregate_modifiers(
            out, distinct, filter, order_by,
        ))),
        None => Err(match refusal(&upper) {
            // A known intrinsic this target cannot express. The message names the
            // function the *reader* wrote and says what the target lacks — the
            // difference between "the compiler has a gap" and "this engine has no
            // such function" is the whole point of refusing rather than guessing.
            Some(why) => why,
            None => format!(
                "unknown SQL intrinsic `{upper}` with {} argument(s)",
                args.len()
            ),
        }),
    }
}

/// Test divisibility *before* producing a fixed-scale quotient. SUM and COUNT
/// have identical DISTINCT/FILTER/OVER clauses. NULLIF also protects an empty
/// window frame. Neither implementation divides using a floating-point type.
fn exact_decimal_avg(to: Dialect, scale: u32, sum: Expr, count: Expr) -> Result<Expr, String> {
    use BinaryOperator::{Eq, Multiply};
    if support(Cap::ExactDecimalAvg, to) == crate::dialect::Support::Unsupported {
        return Err("exact decimal avg is implemented for PostgreSQL and DuckDB only; convert the input to float first if approximation is intended".into());
    }
    // SUM and COUNT must see the same sample, including FILTER and OVER keys.
    let sum = crate::stage::try_transform_deep(sum, &|e| match &e {
        Expr::Function { name, .. }
            if name.eq_ignore_ascii_case("random") || name.eq_ignore_ascii_case("rand") =>
        {
            Err("project random into a column before using it in an exact decimal avg".into())
        }
        _ => Ok(e),
    })?;
    let decimal = |s| DataType::Decimal {
        precision: Some(38),
        scale: Some(s),
    };
    let unit = if scale == 0 {
        "1".into()
    } else {
        format!("0.{}1", "0".repeat(scale as usize - 1))
    };
    let unit = cast(s(&unit), decimal(scale));
    let divisor = func("NULLIF", vec![count.clone(), n("0")]);
    let step = bin(divisor.clone(), Multiply, unit.clone());
    let remainder = func("MOD", vec![sum.clone(), step]);
    let quotient = if to == Dialect::DuckDb {
        // Decimal `/` returns DOUBLE. Read the fixed-scale sum as integer
        // minor units without multiplying a 38-digit value by 10^scale.
        let text = cast(cast(sum.clone(), decimal(scale)), DataType::Varchar(None));
        let digits = func("REPLACE", vec![text, s("."), s("")]);
        let units = cast(
            digits,
            DataType::Dialect {
                kind: sqlglot_rust::ast::DataTypeKind::Int128,
                sql: "HUGEINT".into(),
            },
        );
        cast(func("DIVIDE", vec![units, divisor]), decimal(0))
    } else {
        // PostgreSQL NUMERIC can hold the scaled numerator without a 38-digit cap.
        let factor = n(&format!("1{}", "0".repeat(scale as usize)));
        func("DIV", vec![bin(sum.clone(), Multiply, factor), divisor])
    };
    // The invalid cast depends on the group; a constant error could fail at
    // planning time even for exact or empty groups.
    let inexact = cast(
        func("CONCAT", vec![s("inexact decimal avg: "), sum]),
        decimal(scale),
    );
    Ok(cast(
        Expr::Case {
            operand: None,
            when_clauses: vec![
                (bin(count, Eq, n("0")), Expr::Null),
                (bin(remainder, Eq, n("0")), bin(quotient, Multiply, unit)),
            ],
            else_clause: Some(Box::new(inexact)),
        },
        decimal(scale),
    ))
}

/// Put `DISTINCT` and a `FILTER` back on a call that `intrinsic` rebuilt.
///
/// The intrinsic table answers "what does this function spell on this target",
/// and its answer is a whole `Function` node — so the two aggregate modifiers the
/// language carries on the *call* have to be moved onto the rebuilt one. A
/// non-function result (an array literal, a cast, a `CASE`) has nowhere to put
/// them; that is a bug in the table rather than in the program, so it is left
/// alone rather than silently losing the modifier again.
fn with_aggregate_modifiers(
    e: Expr,
    distinct: bool,
    filter: Option<Box<Expr>>,
    order_by: Vec<OrderByItem>,
) -> Expr {
    if !distinct && filter.is_none() && order_by.is_empty() {
        return e;
    }
    match e {
        Expr::Function {
            name,
            args,
            distinct: existing_distinct,
            filter: existing_filter,
            over,
            order_by: existing_order,
            within_group,
        } => Expr::Function {
            name,
            args,
            distinct: distinct || existing_distinct,
            filter: filter.or(existing_filter),
            over,
            // An ordered aggregate's keys (`arrayAggBy`) travel the same way the
            // modifiers do, and were dropped the same way: the rebuilt call is
            // made from the argument vector, which has no room for them.
            order_by: if order_by.is_empty() {
                existing_order
            } else {
                order_by
            },
            within_group,
        },
        other => other,
    }
}

/// Why a *known* intrinsic cannot be lowered on this target, when that is the reason.
///
/// `None` from [`intrinsic`] means either "no such intrinsic" or "this target cannot
/// express it", and those are different messages. This is where the second one is
/// answered.
///
/// **It currently refuses nothing.** `random`, `reverse`, `listAt`, `lpad`/`rpad` and
/// the whole `format*`/`parse*` family used to be listed here, each because SQLite (or
/// PostgreSQL's codegen) could not spell it directly — and each turned out to have a
/// spelling after all: a mask and a scale, a recursive CTE, a parenthesised subscript, a
/// `ZEROBLOB`, a per-field `strftime`. Refusing made the language's surface depend on
/// `--dialect`, which is the one thing the prelude exists to prevent, so the emulations
/// are the answer and this function is the hook for the next intrinsic that genuinely
/// has none.
fn refusal(name: &str) -> Option<String> {
    let _ = name;
    None
}

/// The format vocabulary the language writes, and how a target spells it.
///
/// `%Y %m %d %H %M %S` is strftime's, which is what DuckDB and BigQuery read
/// directly. The others differ in small, silent ways: PostgreSQL and Snowflake
/// want `YYYY MM DD HH24 MI SS`, Spark and T-SQL `.NET`/Java `yyyy MM dd HH mm
/// ss`, and MySQL's dialect (which Trino's `date_parse` follows) reads `%i` for
/// minutes while `%M` means the month's *name* — so passing a C-style format
/// through unchanged would silently parse minutes as a month name.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FmtStyle {
    /// `%Y %m %d %H %M %S` — DuckDB, BigQuery.
    Strftime,
    /// SQLite's `strftime`: the same numeric specifiers as C, and **no** name
    /// specifiers. It is a separate style rather than folded into `Strftime`
    /// because sharing that one made `%B`/`%A` reach SQLite as-is, where `strftime`
    /// answers NULL instead of failing — so `monthName` returned a column of NULLs
    /// while its type said `string`.
    Sqlite,
    /// PostgreSQL, Snowflake: `YYYY MM DD HH24 MI SS`.
    Pg,
    /// MySQL and Trino's `date_parse`: `%Y %m %d %H %i %s`.
    Mysql,
    /// Spark `to_date`/`date_format` and T-SQL `FORMAT`: `yyyy MM dd HH mm ss`.
    Java,
}

/// Translate the language's format string into the target's spelling.
fn translate_format(fmt: &str, style: FmtStyle) -> Result<String, String> {
    let mut out = String::with_capacity(fmt.len() + 8);
    let mut chars = fmt.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let Some(spec) = chars.next() else {
            return Err(format!(
                "`{fmt}` ends with `%`, which is not a format specifier; write `%%` for a                  literal percent"
            ));
        };
        let token = match (spec, style) {
            // A literal percent. C-style formats (and therefore DuckDB, BigQuery and
            // SQLite) spell it `%%` in the format string; the `TO_CHAR` family
            // (PostgreSQL, Snowflake, ANSI) spells it `%`, as do MySQL and Spark. So
            // this is the one specifier whose translation is not a straight copy, and
            // passing `%` to `strftime` makes DuckDB reject the whole format with
            // "Trailing format character %".
            ('%', FmtStyle::Strftime | FmtStyle::Mysql | FmtStyle::Java) => "%%",
            ('%', FmtStyle::Pg | FmtStyle::Sqlite) => "%",
            ('Y', FmtStyle::Strftime | FmtStyle::Sqlite) => "%Y",
            ('Y', FmtStyle::Mysql) => "%Y",
            ('Y', FmtStyle::Pg) => "YYYY",
            ('Y', FmtStyle::Java) => "yyyy",
            ('m', FmtStyle::Strftime | FmtStyle::Mysql | FmtStyle::Sqlite) => "%m",
            ('m', FmtStyle::Pg) => "MM",
            ('m', FmtStyle::Java) => "MM",
            ('d', FmtStyle::Strftime | FmtStyle::Mysql | FmtStyle::Sqlite) => "%d",
            ('d', FmtStyle::Pg) => "DD",
            ('d', FmtStyle::Java) => "dd",
            ('H', FmtStyle::Strftime | FmtStyle::Mysql | FmtStyle::Sqlite) => "%H",
            ('H', FmtStyle::Pg) => "HH24",
            ('H', FmtStyle::Java) => "HH",
            ('M', FmtStyle::Strftime | FmtStyle::Sqlite) => "%M",
            ('M', FmtStyle::Mysql) => "%i",
            ('M', FmtStyle::Pg) => "MI",
            ('M', FmtStyle::Java) => "mm",
            ('S', FmtStyle::Strftime | FmtStyle::Sqlite) => "%S",
            ('S', FmtStyle::Mysql) => "%s",
            ('S', FmtStyle::Pg) => "SS",
            ('S', FmtStyle::Java) => "ss",
            // Names rather than numbers. `formatDate` only: a month *name* is
            // what a report axis is labelled with, and no engine parses one back
            // the same way, so `parseDate` keeps its numeric set.
            ('b', FmtStyle::Strftime) => "%b",
            ('b', FmtStyle::Mysql) => "%b",
            // PostgreSQL blank-pads a month or weekday name to nine characters, so
            // `%B %d` came out `February  29` and `monthName` carried a trailing
            // space there and nowhere else. `FM` is the unpadded form.
            ('b', FmtStyle::Pg) => "FMMon",
            ('b', FmtStyle::Java) => "MMM",
            ('B', FmtStyle::Strftime) => "%B",
            ('B', FmtStyle::Mysql) => "%M",
            ('B', FmtStyle::Pg) => "FMMonth",
            ('B', FmtStyle::Java) => "MMMM",
            ('a', FmtStyle::Strftime) => "%a",
            ('a', FmtStyle::Mysql) => "%a",
            ('a', FmtStyle::Pg) => "FMDy",
            ('a', FmtStyle::Java) => "EEE",
            ('A', FmtStyle::Strftime) => "%A",
            ('A', FmtStyle::Mysql) => "%W",
            ('A', FmtStyle::Pg) => "FMDay",
            ('A', FmtStyle::Java) => "EEEE",
            ('j', FmtStyle::Strftime | FmtStyle::Sqlite) => "%j",
            ('j', FmtStyle::Mysql) => "%j",
            ('j', FmtStyle::Pg) => "DDD",
            ('j', FmtStyle::Java) => "DDD",
            _ => {
                return Err(format!(
                    "`%{spec}` is not a format specifier this language translates; the \
                     supported ones are `%Y` (year), `%m` (month), `%d` (day), `%H` (hour), \
                     `%M` (minute), `%S` (second), `%b`/`%B` (month name), \
                     `%a`/`%A` (weekday name), `%j` (day of year) and `%%` for a percent"
                ))
            }
        };
        out.push_str(token);
    }
    Ok(out)
}

/// Does a translated format name a time of day? Only BigQuery's `PARSE_DATE`
/// cares, and it is the one that would fail on the server.
fn has_time_element(fmt: &str) -> bool {
    let (mut h, mut m, mut sec) = (0usize, 0usize, 0usize);
    for (i, b) in fmt.bytes().enumerate() {
        if b == b'%' {
            match fmt.as_bytes().get(i + 1) {
                Some(b'H') => h += 1,
                Some(b'M') => m += 1,
                Some(b'S') => sec += 1,
                _ => {}
            }
        }
    }
    h + m + sec > 0
}

/// The style a family's parser and formatter read.
fn fmt_style(f: Fam) -> FmtStyle {
    match f {
        Fam::Duck | Fam::BigQuery => FmtStyle::Strftime,
        // SQLite formats with `strftime`, which reads the C-style *numeric*
        // specifiers but has no name specifiers at all.
        Fam::Sqlite => FmtStyle::Sqlite,
        Fam::Trino | Fam::Mysql => FmtStyle::Mysql,
        Fam::Spark | Fam::Tsql => FmtStyle::Java,
        Fam::Ansi | Fam::Snowflake => FmtStyle::Pg,
    }
}

/// `parseDate` / `formatDate`: one interface, the target's own function
/// underneath.
///
/// `args[0]` is the format and `args[1]` the value (`parseDate` takes the string
/// to parse there, `formatDate` the date to print). The format must be a
/// literal: it is translated at compile time, and a runtime string could not
/// be.
fn parse_or_format(name: &str, args: &[Expr], f: Fam) -> Result<Expr, String> {
    let (parse, timestamp) = match name {
        "CAGARA_PARSE_DATE" => (true, false),
        "CAGARA_PARSE_TIMESTAMP" => (true, true),
        "CAGARA_FORMAT_DATE" => (false, false),
        _ => (false, true),
    };
    let [fmt, value] = args else {
        return Err(format!(
            "unknown SQL intrinsic `{name}` with {} argument(s)",
            args.len()
        ));
    };
    let Expr::StringLiteral(fmt) = fmt else {
        return Err(format!(
            "`{}` takes a literal format string — `\"%Y%m%d\"`, not a column or an \
             expression — because it is translated for the target at compile time",
            if parse {
                if timestamp {
                    "parseTimestamp"
                } else {
                    "parseDate"
                }
            } else if timestamp {
                "formatTimestamp"
            } else {
                "formatDate"
            }
        ));
    };
    // SQLite is handled before translation, not through it: its `strftime` reads the
    // numeric C specifiers but has no name specifiers, and refusing them would make
    // the language depend on `--dialect`. The format is a compile-time literal, so it
    // is decomposed instead — see `sqlite_format`/`sqlite_parse`.
    if f == Fam::Sqlite {
        return if parse {
            slice_parse(fmt, value, timestamp, f)
        } else {
            sqlite_format(fmt, value, timestamp)
        };
    }
    // T-SQL's `PARSE` is culture-driven and it has no format-driven cast, so the same
    // slice technique applies: the format is a literal, so `%Y-%m-%d` is three
    // `SUBSTRING`s joined with `-` and cast. It can also *format* by pattern
    // (`FORMAT`), so only the parse half needs this.
    if f == Fam::Tsql && parse {
        return slice_parse(fmt, value, timestamp, f);
    }
    let style = fmt_style(f);
    let fmt = translate_format(fmt, style)?;
    let f_lit = || Expr::StringLiteral(fmt.clone());
    // BigQuery's `PARSE_DATE` reads date elements only: a time element belongs
    // to `PARSE_TIMESTAMP`, and BigQuery rejects the call rather than dropping
    // the time.
    if parse && !timestamp && f == Fam::BigQuery && has_time_element(&fmt) {
        return Err(
            "BigQuery's `PARSE_DATE` reads only date elements, and this format has a \
                    time of day; use `parseTimestamp` for `%H`, `%M` or `%S`"
                .to_string(),
        );
    }
    if parse {
        let parsed = match f {
            Fam::Duck => func("STRPTIME", vec![value.clone(), f_lit()]),
            Fam::Trino => func("DATE_PARSE", vec![value.clone(), f_lit()]),
            Fam::Mysql => func("STR_TO_DATE", vec![value.clone(), f_lit()]),
            Fam::BigQuery if timestamp => func("PARSE_TIMESTAMP", vec![f_lit(), value.clone()]),
            Fam::BigQuery => func("PARSE_DATE", vec![f_lit(), value.clone()]),
            Fam::Snowflake | Fam::Ansi if timestamp => {
                func("TO_TIMESTAMP", vec![value.clone(), f_lit()])
            }
            Fam::Snowflake | Fam::Ansi => func("TO_DATE", vec![value.clone(), f_lit()]),
            Fam::Spark if timestamp => func("TO_TIMESTAMP", vec![value.clone(), f_lit()]),
            Fam::Spark => func("TO_DATE", vec![value.clone(), f_lit()]),
            // SQLite and T-SQL have no format parser: SQLite reads only ISO
            // strings, and T-SQL's `PARSE` is culture-driven.
            // Unreachable: `Sqlite` and `Tsql` both return from `slice_parse` at the
            // top of this function, because neither has a format-driven cast and the
            // format is a literal that can be sliced. Kept as an explicit panic rather
            // than a catch-all so that a future arm added above cannot silently fall
            // through into the wrong spelling.
            Fam::Sqlite | Fam::Tsql => {
                unreachable!("slice_parse handles this target before translation")
            }
        };
        // Only the date form narrows the time of day away; a timestamp is the
        // parser's own answer.
        Ok(if timestamp {
            parsed
        } else {
            cast(parsed, DataType::Date)
        })
    } else if timestamp {
        Ok(match f {
            Fam::Duck => func("STRFTIME", vec![value.clone(), f_lit()]),
            // SQLite's `strftime(FORMAT, TIME, …)` takes the format **first**, unlike
            // DuckDB's `strftime(TIME, FORMAT)`. Sharing DuckDB's arm passed the time
            // as the format and the format as a bogus time, and SQLite answers NULL
            // for an unparseable time rather than rejecting the statement — so
            // `formatTimestamp` returned a column of NULLs while its type said
            // `string`. It is spelled out here rather than folded into the `Duck` arm
            // because that arm is the one that is wrong for this engine.
            Fam::Sqlite => func("STRFTIME", vec![f_lit(), value.clone()]),
            Fam::Trino | Fam::Mysql | Fam::Spark => {
                func("DATE_FORMAT", vec![value.clone(), f_lit()])
            }
            Fam::BigQuery => func("FORMAT_TIMESTAMP", vec![f_lit(), value.clone()]),
            Fam::Snowflake | Fam::Ansi => func("TO_CHAR", vec![value.clone(), f_lit()]),
            Fam::Tsql => func("FORMAT", vec![value.clone(), f_lit()]),
        })
    } else {
        Ok(match f {
            Fam::Duck => func("STRFTIME", vec![value.clone(), f_lit()]),
            Fam::Trino | Fam::Mysql => func("DATE_FORMAT", vec![value.clone(), f_lit()]),
            Fam::BigQuery => func("FORMAT_DATE", vec![f_lit(), value.clone()]),
            Fam::Snowflake | Fam::Ansi => func("TO_CHAR", vec![value.clone(), f_lit()]),
            Fam::Spark => func("DATE_FORMAT", vec![value.clone(), f_lit()]),
            Fam::Sqlite => func("STRFTIME", vec![f_lit(), value.clone()]),
            Fam::Tsql => func("FORMAT", vec![value.clone(), f_lit()]),
        })
    }
}

/// The message a dialect without an array type gives. A `list` has no SQL type
/// there, and pretending it is JSON text would change the type without saying
/// so.
///
/// The target is named: a refusal is advice, and advice that does not say which
/// `--dialect` produced it leaves the reader guessing which of their flags to change.
fn no_list(name: &str, d: Dialect) -> String {
    format!(
        "`{name}` needs a dialect with an array type, and `{}` has none; \
         `list` values are PostgreSQL and DuckDB only",
        crate::dialect::name(d)
    )
}

/// The dialects that can express a `list` at all.
fn has_list(d: Dialect) -> bool {
    support(Cap::Arrays, d).emits()
}

/// The dialects that can hold an exact decimal.
///
/// SQLite's `NUMERIC` affinity stores what it is given as INTEGER or REAL, so a
/// value cast to `DECIMAL(10, 2)` there is a binary float wearing the type's
/// name — the one thing this type exists to prevent. Refusing is the honest
/// answer, the same way `list` and `try*` are refused.
fn has_decimal(d: Dialect) -> bool {
    support(Cap::ExactDecimal, d).emits()
}

/// The refusal for a decimal cast on a dialect that cannot hold one.
fn decimal_refusal(name: &str, d: Dialect) -> Option<String> {
    let what = match name {
        "CAGARA_TO_DECIMAL" => "`toDecimal`",
        "CAGARA_TRY_DECIMAL" => "`tryDecimal`",
        _ => return None,
    };
    (!has_decimal(d)).then(|| {
        format!(
            "{what} needs an exact decimal type, and this dialect has none: SQLite's \
             NUMERIC affinity stores the value as REAL or INTEGER, so the precision \
             would be lost silently. Use `toFloat` if a binary float is what the value \
             is, and PostgreSQL, DuckDB, MySQL, Trino, Spark, BigQuery, Snowflake or \
             T-SQL for money"
        )
    })
}

/// The refusal for `unnest`, which is a relational node rather than a template:
/// the lowerer emits its SQL directly, so it asks here for the wording.
pub(crate) fn unnest_refusal(d: Dialect) -> Option<String> {
    list_refusal("CAGARA_UNNEST", d)
}

/// The refusal for a `list` intrinsic on a dialect with no array type.
///
/// Checked before the intrinsic is lowered, so the message is about the target
/// rather than about an unknown name: `list` is a type the language has, and
/// this engine is the thing that cannot hold it.
fn list_refusal(name: &str, d: Dialect) -> Option<String> {
    let list_intrinsic = match name {
        "CAGARA_LIST" => "a list literal",
        "CAGARA_ARRAY_AGG" => "`arrayAgg`",
        "CAGARA_LIST_CONCAT" => "`<>` on a list",
        "CAGARA_LIST_LENGTH" => "`listLength`",
        "CAGARA_LIST_CONTAINS" => "`listContains`",
        "CAGARA_LIST_AT" => "`listAt`",
        "CAGARA_UNNEST" => "`unnest`",
        _ => return None,
    };
    (!has_list(d)).then(|| no_list(list_intrinsic, d))
}

/// `LPAD(s, n, fill)` from the language's subject-last `lpad n fill s`.
///
/// SQLite has neither `LPAD` nor `RPAD` nor `REPEAT`, but `HEX(ZEROBLOB(n))` is the
/// character `0` repeated `2n` times, so replacing each `0` with the fill gives at
/// least `n` copies — enough for any `n`, since `SUBSTR` takes only the missing part:
///
/// ```text
/// CASE WHEN n <= 0         THEN ''                 -- LPAD truncates to nothing
///      WHEN LENGTH(s) >= n THEN SUBSTR(s, 1, n)    -- LPAD truncates past n
///      ELSE SUBSTR(padding, 1, n - LENGTH(s)) || s END
/// ```
///
/// Checked against DuckDB's `LPAD` on the growing, truncating, zero-width and
/// empty-fill cases.
fn pad(name: &str, a: &[Expr], f: Fam) -> Expr {
    let (subject, width, fill) = (a[2].clone(), a[0].clone(), a[1].clone());
    if f != Fam::Sqlite {
        let pad = func(name, vec![subject.clone(), width.clone(), fill.clone()]);
        // DuckDB *raises* on an empty fill where PostgreSQL and SQLite return the
        // string unpadded — measured. An empty fill can pad nothing, so returning the
        // subject is the answer the other two give; the guard makes DuckDB agree
        // instead of failing a query that type-checks.
        if f == Fam::Duck {
            return case(
                bin(
                    atomic(func("LENGTH", vec![fill])),
                    BinaryOperator::Eq,
                    n("0"),
                ),
                subject,
                pad,
            );
        }
        return pad;
    }
    let missing = bin(
        atomic(width.clone()),
        BinaryOperator::Minus,
        func("LENGTH", vec![subject.clone()]),
    );
    let zeros = func("HEX", vec![func("ZEROBLOB", vec![atomic(width.clone())])]);
    let padding = func("REPLACE", vec![zeros, s("0"), fill]);
    let grown = func("SUBSTR", vec![atomic(padding), n("1"), atomic(missing)]);
    let glued = if name == "LPAD" {
        bin(
            atomic(grown),
            BinaryOperator::Concat,
            atomic(subject.clone()),
        )
    } else {
        bin(
            atomic(subject.clone()),
            BinaryOperator::Concat,
            atomic(grown),
        )
    };
    let truncated = func(
        "SUBSTR",
        vec![subject.clone(), n("1"), atomic(width.clone())],
    );
    case(
        bin(atomic(width.clone()), BinaryOperator::LtEq, n("0")),
        s(""),
        case(
            bin(
                func("LENGTH", vec![subject]),
                BinaryOperator::GtEq,
                atomic(width),
            ),
            truncated,
            glued,
        ),
    )
}

/// `s LIKE p`, or `s ILIKE p` when `ci`.
fn like(s: Expr, p: Expr, ci: bool) -> Expr {
    let (s, p) = (Box::new(s), Box::new(p));
    if ci {
        Expr::ILike {
            expr: s,
            pattern: p,
            negated: false,
            escape: None,
        }
    } else {
        Expr::Like {
            expr: s,
            pattern: p,
            negated: false,
            escape: None,
        }
    }
}

/// `s GLOB p'`, where `p'` is the `LIKE` pattern `p` with its metacharacters
/// translated. SQLite's case-sensitive equivalent of `LIKE`.
fn glob(subject: Expr, pattern: Expr) -> Expr {
    // REPLACE order is load-bearing: `[` is escaped first because the escapes for
    // `*` and `[` introduce `[`, then the literal `*`/`?` are escaped before `%`/`_`
    // are widened into exactly those characters.
    let mut pat = pattern;
    for (from, to) in [("[", "[[]"), ("*", "[*]"), ("%", "*"), ("_", "?")] {
        pat = func("REPLACE", vec![pat, s(from), s(to)]);
    }
    // `GLOB` exists both as an infix operator (`subject GLOB pattern`) and as a
    // function, and the function takes the **pattern first** — `glob(pattern,
    // subject)`, the opposite of the operator. Emitting the function with the
    // operator's order silently answered "no match" for every pattern that was not
    // also a valid GLOB, which is the whole reason the translation exists.
    func("GLOB", vec![pat, subject])
}

fn intrinsic(name: &str, a: &[Expr], f: Fam) -> Option<Expr> {
    let lit = |i: usize| match a.get(i) {
        Some(Expr::StringLiteral(s)) => Some(s.to_ascii_uppercase()),
        _ => None,
    };
    let ts = |i: usize| lit(i).map(|k| k == "TIMESTAMP");
    // A precision or scale has to be a literal: the type it builds is part of
    // the statement's syntax, so it cannot be an expression the backend is
    // still computing.
    let num = |i: usize| match a.get(i) {
        Some(Expr::Number(v)) => v.parse::<u32>().ok(),
        _ => None,
    };
    Some(match (name, a.len()) {
        ("CAGARA_ADD", 4) => add(ts(0)?, &lit(1)?, date_arg(a[2].clone(), f), a[3].clone(), f)?,
        ("CAGARA_TRUNC", 3) => trunc(ts(0)?, &lit(1)?, date_arg(a[2].clone(), f), f)?,
        ("CAGARA_PART", 3) => part(ts(0)?, &lit(1)?, a[2].clone(), f)?,
        ("CAGARA_DAYS_BETWEEN", 2) => {
            days_between(date_arg(a[0].clone(), f), date_arg(a[1].clone(), f), f)
        }
        ("CAGARA_NOW", 0) => match f {
            Fam::Ansi => kw("CURRENT_TIMESTAMP"),
            Fam::Sqlite => func("DATETIME", vec![s("now")]),
            _ => Expr::TypedFunction {
                func: TypedFunction::CurrentTimestamp,
                filter: None,
                over: None,
            },
        },
        ("CAGARA_TO_DATE", 1) => match f {
            Fam::Sqlite => func("DATE", vec![a[0].clone()]),
            _ => cast(a[0].clone(), DataType::Date),
        },
        ("CAGARA_TO_TIMESTAMP", 1) => match f {
            Fam::Sqlite => func("DATETIME", vec![a[0].clone()]),
            Fam::Mysql => cast(a[0].clone(), DataType::DateTime),
            _ => cast(a[0].clone(), timestamp()),
        },
        ("CAGARA_TO_STRING", 1) => match f {
            // MySQL's CAST accepts CHAR, not TEXT / VARCHAR.
            Fam::Mysql => cast(a[0].clone(), DataType::Char(None)),
            Fam::Trino => cast(a[0].clone(), DataType::Varchar(None)),
            Fam::Spark => cast(a[0].clone(), DataType::String),
            _ => cast(a[0].clone(), DataType::Text),
        },
        // `CAGARA_TO_DECIMAL(x, precision, scale)`. The precision and scale
        // are separate arguments rather than being written into the type in
        // the template: `CAST(x AS DECIMAL($1, $2))` cannot be parsed, because
        // the template is re-read after substitution and a type's precision is
        // syntax, not an expression.
        //
        // The language's type for it is `decimal p s` (the parameters come from
        // the two literals the call is written with).
        ("CAGARA_TO_DECIMAL", 3) => cast(
            a[0].clone(),
            DataType::Decimal {
                precision: num(1),
                scale: num(2),
            },
        ),
        // Widths follow the language: `int` is 64-bit, so a cast to it is
        // `BIGINT`, not the 32-bit `INT` that PostgreSQL, MySQL, DuckDB and
        // T-SQL would otherwise produce. `float` is an IEEE double, so the
        // target is `DOUBLE PRECISION` / `DOUBLE` / `FLOAT` / `FLOAT64` as the
        // engine spells it.
        ("CAGARA_TO_INT", 1) => cast(a[0].clone(), DataType::BigInt),
        ("CAGARA_TRUNC_INT", 1) => trunc_int(a[0].clone(), f),
        // `LOG10` is the spelling on DuckDB, SQLite, MariaDB, Trino and Snowflake;
        // these targets name it otherwise, and one has no base-10 logarithm at all.
        ("CAGARA_LOG10", 1) => log10(a[0].clone(), f)?,
        ("CAGARA_LOG2", 1) => log2(a[0].clone(), f)?,
        ("CAGARA_RANDOM", 0) => random(f)?,
        ("CAGARA_ASCII", 1) => match f {
            // The name differs and the *answer* is the same: the code point of the
            // first character. SQLite and Trino call it `UNICODE`/`CODEPOINT`, which
            // is the more honest name — `ASCII` implies a byte, and the function
            // returns a code point.
            Fam::Sqlite => func("UNICODE", vec![a[0].clone()]),
            // Trino's `codepoint` is declared `codepoint(varchar(1))` and its type
            // check runs before any constant folding — measured: `codepoint(substr(s,
            // 1, 1))` is rejected as `varchar`, not as `varchar(1)`. The cast is what
            // says the length, and without it there is no spelling of this function on
            // Trino at all.
            Fam::Trino => func(
                "CODEPOINT",
                vec![cast(
                    func(
                        "SUBSTR",
                        vec![
                            a[0].clone(),
                            Expr::Number("1".into()),
                            Expr::Number("1".into()),
                        ],
                    ),
                    DataType::UserDefined("VARCHAR(1)".into()),
                )],
            ),
            _ => func("ASCII", vec![a[0].clone()]),
        },
        ("CAGARA_CHR", 1) => match f {
            Fam::Sqlite => func("CHAR", vec![a[0].clone()]),
            _ => func("CHR", vec![a[0].clone()]),
        },
        // The prelude's template already puts the string first (`CAGARA_REPEAT($2, $1)`,
        // because the language's `repeat` takes the count first like every stage), so
        // the intrinsic receives `(s, n)` — the order every target wants.
        ("CAGARA_REPEAT", 2) => repeat(a[0].clone(), a[1].clone(), f)?,
        ("CAGARA_REVERSE", 1) => reverse(a[0].clone(), f)?,
        ("CAGARA_TO_FLOAT", 1) => cast(a[0].clone(), float_type(f)),
        // `TRY_CAST` cannot be spelled as a template: sqlglot's parser reads
        // `TRY_CAST(x AS INT)` as `Cast`, dropping the "try" before the
        // backend ever sees it, so a template would silently become a hard
        // cast that fails on the first unparseable row. The node has to be
        // built here instead. SQLite has no equivalent; see `try_cast`.
        ("CAGARA_TRY_INT", 1) => try_cast_for(a[0].clone(), DataType::BigInt, f),
        ("CAGARA_TRY_FLOAT", 1) => try_cast_for(a[0].clone(), float_type(f), f),
        ("CAGARA_TRY_DATE", 1) => try_cast_for(a[0].clone(), DataType::Date, f),
        ("CAGARA_TRY_TIMESTAMP", 1) => try_cast_for(a[0].clone(), timestamp(), f),
        ("CAGARA_TRY_DECIMAL", 3) => try_cast(
            a[0].clone(),
            DataType::Decimal {
                precision: num(1),
                scale: num(2),
            },
        ),
        // `LEFT(s, n)` is the first `n` characters, and a *negative* `n` drops that
        // many from the end (`LEFT('abcde', -1)` is `'abcd'`). SQLite has no `LEFT`,
        // and `SUBSTR(s, 1, n)` reads a negative length as "up to n before the end",
        // which is `''` — so the emulation answered a different question for every
        // negative `n` while looking correct for the positive ones. The `n < 0` arm
        // below is what `LEFT` means; `LENGTH(s) + n` is the prefix that remains.
        ("CAGARA_LEFT", 2) => match f {
            Fam::Sqlite => {
                let (x, k) = (a[0].clone(), a[1].clone());
                let positive = func("SUBSTR", vec![x.clone(), n("1"), k.clone()]);
                let negative = func(
                    "SUBSTR",
                    vec![
                        x.clone(),
                        n("1"),
                        bin(func("LENGTH", vec![x]), BinaryOperator::Plus, k.clone()),
                    ],
                );
                case(bin(k, BinaryOperator::Lt, n("0")), negative, positive)
            }
            Fam::Trino => func("SUBSTR", vec![a[0].clone(), n("1"), a[1].clone()]),
            _ => func("LEFT", a.to_vec()),
        },
        ("CAGARA_RIGHT", 2) => match f {
            // `RIGHT(s, n)` is the last `n` characters, and a negative `n` drops that
            // many from the *front* (`RIGHT('abcde', -1)` is `'bcde'`). Both arms are
            // needed: `SUBSTR(s, -n, n)` is the positive case, and for `n < 0` the
            // remaining tail starts at `1 - n` (`SUBSTR(s, 2)` for `n = -1`). The old
            // guard turned every `n <= 0` into `''`, which is `RIGHT`'s answer only
            // at zero.
            Fam::Sqlite => {
                let (x, k) = (a[0].clone(), a[1].clone());
                let positive = func("SUBSTR", vec![x.clone(), neg(k.clone()), k.clone()]);
                let negative = func(
                    "SUBSTR",
                    vec![x.clone(), bin(n("1"), BinaryOperator::Minus, k.clone())],
                );
                case(bin(k.clone(), BinaryOperator::Lt, n("0")), negative, {
                    case(bin(k, BinaryOperator::Eq, n("0")), s(""), positive)
                })
            }
            // No RIGHT: start n characters from the end (past it for n <= 0).
            Fam::Trino => {
                let (x, k) = (a[0].clone(), a[1].clone());
                let from = bin(
                    bin(
                        func("LENGTH", vec![x.clone()]),
                        BinaryOperator::Minus,
                        atomic(k),
                    ),
                    BinaryOperator::Plus,
                    n("1"),
                );
                func("SUBSTR", vec![x, func("GREATEST", vec![from, n("1")])])
            }
            _ => func("RIGHT", a.to_vec()),
        },
        // sqlglot spells LENGTH as LEN for BigQuery (which has no LEN); MySQL's
        // LENGTH counts bytes; T-SQL's LEN ignores trailing spaces, so it
        // measures `s + 'x'` instead.
        ("CAGARA_LENGTH", 1) => match f {
            Fam::Mysql => func("CHAR_LENGTH", a.to_vec()),
            Fam::Tsql => {
                let padded = bin(atomic(a[0].clone()), BinaryOperator::Plus, s("x"));
                atomic(bin(
                    func("LEN", vec![padded]),
                    BinaryOperator::Minus,
                    n("1"),
                ))
            }
            _ => func("LENGTH", a.to_vec()),
        },
        // `LPAD`/`RPAD` are the same call everywhere they exist; they are named here
        // only so that `refusal` can identify them and refuse on SQLite. Emitting
        // `LPAD(...)` there produced `no such function: LPAD` from the engine at run
        // time, which is a worse error than the compiler's own.
        // `None` for SQLite hands the question to `refusal`, which names the function
        // and the target. Emitting the call there produced `no such function: LPAD`
        // from the engine at run time instead.
        ("CAGARA_LPAD", 3) => pad("LPAD", a, f),
        ("CAGARA_RPAD", 3) => pad("RPAD", a, f),
        ("CAGARA_STRPOS", 2) => {
            let (x, sub) = (a[0].clone(), a[1].clone());
            match f {
                Fam::Mysql => func("LOCATE", vec![sub, x]),
                Fam::Sqlite | Fam::Spark => func("INSTR", vec![x, sub]),
                Fam::Tsql | Fam::Snowflake => func("CHARINDEX", vec![sub, x]),
                _ => func("STRPOS", vec![x, sub]),
            }
        }
        // SQLite's `LIKE` is case-insensitive for ASCII unless the connection was
        // opened with `PRAGMA case_sensitive_like = ON`, so emitting it there makes
        // `like` quietly mean `ilike` — the same program returns different rows on
        // SQLite than on every other target, which is the thing the prelude is
        // supposed to prevent. `GLOB` is the case-sensitive matcher SQLite does have,
        // and a `LIKE` pattern maps onto it exactly once the metacharacters are
        // translated: `%` -> `*`, `_` -> `?`, and a literal `*`, `?` or `[` in the
        // pattern escaped so it stays literal. The escape order matters — `[` first,
        // because the escapes themselves introduce `[`, and `*`/`?` before `%`/`_`
        // are widened into them.
        ("CAGARA_LIKE", 2) => match f {
            Fam::Sqlite => glob(a[1].clone(), a[0].clone()),
            _ => like(a[1].clone(), a[0].clone(), false),
        },
        // `ILIKE` is `LIKE` with both sides folded. No SQLite spelling of `LOWER`
        // is case-insensitive beyond ASCII, so neither is `ilike` there; that limit
        // is shared with `upper`/`lower` and stated in `docs/SQL-DIALECTS.md`.
        ("CAGARA_ILIKE", 2) => match f {
            Fam::Sqlite => like(
                func("LOWER", vec![a[1].clone()]),
                func("LOWER", vec![a[0].clone()]),
                false,
            ),
            _ => like(a[1].clone(), a[0].clone(), true),
        },
        ("CAGARA_IDIV", 2) => idiv(a[0].clone(), a[1].clone(), f),
        // The divisor is guarded for the same reason as `/`: the operator is
        // typed non-null, so a zero divisor must answer NULL everywhere rather
        // than raise on one target. BigQuery's `MOD` yields NULL on zero already.
        ("CAGARA_MOD", 2) => match f {
            Fam::BigQuery => func("MOD", a.to_vec()),
            _ => bin(
                atomic(a[0].clone()),
                BinaryOperator::Modulo,
                atomic(guarded_divisor(a[1].clone(), f)),
            ),
        },
        // ── arrays ─────────────────────────────────────────────────────
        //
        // `list` is a value type, and these three are its syntax: a literal,
        // an aggregate that builds one, and concatenation. PostgreSQL and
        // DuckDB have arrays; the dialects that do not refuse the query here
        // rather than emit SQL their engine would reject.
        // PostgreSQL spells a list literal `ARRAY[…]`; DuckDB spells it
        // `list_value(…)`, which is the same value and does not depend on
        // `ARRAY[…]` being accepted there.
        ("CAGARA_LIST", _) => match f {
            Fam::Duck => func("list_value", a.to_vec()),
            _ => Expr::ArrayLiteral(a.to_vec()),
        },
        ("CAGARA_ARRAY_AGG", 1) => match f {
            Fam::Duck => func("list", a.to_vec()),
            _ => func("ARRAY_AGG", a.to_vec()),
        },
        ("CAGARA_LIST_LENGTH", 1) => match f {
            Fam::Duck => func("len", a.to_vec()),
            _ => func("cardinality", a.to_vec()),
        },
        ("CAGARA_LIST_AT", 2) => match f {
            // DuckDB has `list_extract(xs, i)`, which is 1-based and NULL when the
            // list is shorter — measured. That is exactly what `maybe a` promises, so
            // no guard is needed.
            //
            // DuckDB has `list_extract(xs, i)`, which is 1-based and NULL when the
            // list is shorter — measured. That is exactly what `maybe a` promises.
            //
            // PostgreSQL would need the subscript `xs[i]`, and `sqlglot-rust` 0.11.2 can
            // now *build* that node (`Expr::ArrayIndex`) but not *parenthesise* it:
            // `(ARRAY_AGG(id)[2])` is what it generates, and PostgreSQL rejects it with
            // `syntax error at or near "["` — the aggregate has to be wrapped,
            // `(ARRAY_AGG(id))[2]`. Measured on PGlite: the wrapped form is `20`. Since
            // the only list a query can subscript is usually an aggregate's result,
            // emitting the unwrapped form would be SQL that runs nowhere.
            Fam::Duck => func("list_extract", vec![a[0].clone(), a[1].clone()]),
            // PostgreSQL *does* have the subscript and `list` here is a real array
            // type; the only obstacle was codegen. `sqlglot-rust` writes `expr[i]`
            // directly, and `ARRAY_AGG(id)[2]` is a syntax error there — the aggregate
            // has to be parenthesised first, `(ARRAY_AGG(id))[2]`. `Expr::Nested` is
            // that parenthesis, so the subscript is built around it. Measured on
            // PGlite: the wrapped form answers `20`, and the unwrapped one does not
            // parse.
            Fam::Ansi => Expr::ArrayIndex {
                expr: Box::new(Expr::Nested(Box::new(atomic(a[0].clone())))),
                index: Box::new(atomic(a[1].clone())),
            },
            // A target with no array type never reaches here: `list_refusal` refuses
            // the whole query first, with a message naming what needs an array. Listed
            // so the match is exhaustive rather than falling into a catch-all that
            // would silently emit `list_extract` for MySQL.
            //
            // Trino is here because `has_list` does not include it. Trino *does* have
            // `ARRAY_AGG` and `element_at` — measured — so `has_list` is narrower than
            // the truth; widening it is a separate change, because it would turn every
            // existing `arrayAgg` on Trino from a refusal into a query, and that
            // deserves its own tests rather than riding along with `listAt`.
            // PostgreSQL is here for the parenthesisation reason above, not for want of
            // an array type or a subscript.
            Fam::Mysql
            | Fam::Sqlite
            | Fam::Tsql
            | Fam::BigQuery
            | Fam::Snowflake
            | Fam::Spark
            | Fam::Trino => return None,
        },
        ("CAGARA_LIST_CONTAINS", 2) => match f {
            Fam::Duck => func("list_contains", a.to_vec()),
            // `x = ANY(list)` is the standard spelling; it is a comparison
            // rather than a function call, so it is built as one.
            _ => Expr::AnyOp {
                expr: Box::new(a[1].clone()),
                op: BinaryOperator::Eq,
                right: Box::new(a[0].clone()),
            },
        },
        ("CAGARA_LIST_CONCAT", 2) => match f {
            Fam::Duck => func("list_concat", a.to_vec()),
            _ => Expr::BinaryOp {
                left: Box::new(a[0].clone()),
                op: BinaryOperator::Concat,
                right: Box::new(a[1].clone()),
            },
        },
        _ => return None,
    })
}

// ── date arithmetic ────────────────────────────────────────────────────────

fn add(ts: bool, unit: &str, x: Expr, count: Expr, f: Fam) -> Option<Expr> {
    // Only a unit that contributes a whole number of days or months: the
    // prelude spells weeks and quarters in terms of these.
    let field = field(unit)?;
    if !ts && !matches!(unit, "DAY" | "MONTH" | "YEAR") {
        return None;
    }
    let count = atomic(count);
    let same = |e: Expr| cast(atomic(e), if ts { timestamp() } else { DataType::Date });
    Some(match f {
        // `date + interval` is a timestamp in Postgres: cast back.
        Fam::Ansi => same(bin(
            atomic(x),
            BinaryOperator::Plus,
            bin(
                count,
                BinaryOperator::Multiply,
                Expr::Interval {
                    value: Box::new(s("1")),
                    unit: Some(field),
                },
            ),
        )),
        Fam::Mysql => func(
            "DATE_ADD",
            vec![
                x,
                Expr::Interval {
                    value: Box::new(count),
                    unit: Some(field),
                },
            ],
        ),
        Fam::Sqlite => {
            let plural = format!(" {}s", unit.to_ascii_lowercase());
            let modifier = bin(count, BinaryOperator::Concat, s(&plural));
            let mut args = vec![x, modifier];
            // Past the end of a shorter month: the last day, as elsewhere
            // (Jan 31 + 1 month = Feb 29), not an overflow into the next
            // month. `floor` needs SQLite 3.46.
            if matches!(unit, "MONTH" | "YEAR") {
                args.push(s("floor"));
            }
            func(if ts { "DATETIME" } else { "DATE" }, args)
        }
        Fam::Duck => {
            let to = format!("TO_{unit}S");
            same(bin(atomic(x), BinaryOperator::Plus, func(&to, vec![count])))
        }
        Fam::Tsql | Fam::Snowflake => func("DATEADD", vec![kw(unit), count, x]),
        // Same type as `x` for dates and timestamps.
        Fam::Trino => func("DATE_ADD", vec![s(&unit.to_ascii_lowercase()), count, x]),
        Fam::Spark => match (ts, unit) {
            (false, "DAY") => func("DATE_ADD", vec![x, count]),
            (false, "MONTH") => func("ADD_MONTHS", vec![x, count]),
            (false, _) => func(
                "ADD_MONTHS",
                vec![x, bin(count, BinaryOperator::Multiply, n("12"))],
            ),
            // make_interval(years, months, weeks, days, hours, mins, secs)
            (true, u) => {
                // A unit without its own slot would silently land in the
                // seconds slot (a `WEEK` becomes a second), so reject it
                // instead; the prelude spells weeks and quarters in terms of
                // days and months.
                let slot = match u {
                    "YEAR" => 0,
                    "MONTH" => 1,
                    "DAY" => 3,
                    "HOUR" => 4,
                    "MINUTE" => 5,
                    "SECOND" => 6,
                    _ => return None,
                };
                let mut args = vec![n("0"); 7];
                args[slot] = count;
                bin(atomic(x), BinaryOperator::Plus, func("MAKE_INTERVAL", args))
            }
        },
        Fam::BigQuery => {
            let iv = Expr::Interval {
                value: Box::new(count),
                unit: Some(field),
            };
            match (ts, unit) {
                (false, _) => func("DATE_ADD", vec![x, iv]),
                // TIMESTAMP_ADD stops at DAY; months and years go via DATETIME.
                (true, "MONTH" | "YEAR") => {
                    let civil = func("DATETIME_ADD", vec![cast(x, DataType::DateTime), iv]);
                    cast(civil, timestamp())
                }
                (true, _) => func("TIMESTAMP_ADD", vec![x, iv]),
            }
        }
    })
}

fn trunc(ts: bool, unit: &str, x: Expr, f: Fam) -> Option<Expr> {
    let ok = match unit {
        "YEAR" | "QUARTER" | "MONTH" | "WEEK" => true,
        "DAY" | "HOUR" | "MINUTE" => ts,
        _ => false,
    };
    if !ok {
        return None;
    }
    let ty = || if ts { timestamp() } else { DataType::Date };
    Some(match f {
        // Postgres truncates a date as a timestamp: cast back.
        Fam::Ansi | Fam::Duck => cast(func("DATE_TRUNC", vec![s(unit), x]), ty()),
        Fam::Snowflake | Fam::Trino => func("DATE_TRUNC", vec![s(unit), x]),
        // TRUNC keeps a date a date; DATE_TRUNC is for timestamps.
        Fam::Spark if ts => func("DATE_TRUNC", vec![s(unit), x]),
        Fam::Spark => func("TRUNC", vec![x, s(unit)]),
        Fam::Tsql => func(
            "DATETRUNC",
            vec![kw(if unit == "WEEK" { "ISO_WEEK" } else { unit }), x],
        ),
        Fam::BigQuery => {
            let u = kw(if unit == "WEEK" { "ISOWEEK" } else { unit });
            func(
                if ts { "TIMESTAMP_TRUNC" } else { "DATE_TRUNC" },
                vec![x, u],
            )
        }
        Fam::Mysql => {
            let back = |e: Expr| {
                cast(
                    e,
                    if ts {
                        DataType::DateTime
                    } else {
                        DataType::Date
                    },
                )
            };
            match unit {
                // Monday of the ISO week: WEEKDAY is 0 for Monday.
                "WEEK" => back(func(
                    "DATE_SUB",
                    vec![
                        func("DATE", vec![x.clone()]),
                        Expr::Interval {
                            value: Box::new(func("WEEKDAY", vec![x])),
                            unit: Some(DateTimeField::Day),
                        },
                    ],
                )),
                "QUARTER" => {
                    let jan1 = func("MAKEDATE", vec![func("YEAR", vec![x.clone()]), n("1")]);
                    let months = bin(
                        atomic(bin(func("QUARTER", vec![x]), BinaryOperator::Minus, n("1"))),
                        BinaryOperator::Multiply,
                        n("3"),
                    );
                    back(func(
                        "DATE_ADD",
                        vec![
                            jan1,
                            Expr::Interval {
                                value: Box::new(atomic(months)),
                                unit: Some(DateTimeField::Month),
                            },
                        ],
                    ))
                }
                u => {
                    let fmt = match u {
                        "YEAR" => "%Y-01-01",
                        "MONTH" => "%Y-%m-01",
                        "DAY" => "%Y-%m-%d",
                        "HOUR" => "%Y-%m-%d %H:00:00",
                        _ => "%Y-%m-%d %H:%i:00",
                    };
                    back(func("DATE_FORMAT", vec![x, s(fmt)]))
                }
            }
        }
        Fam::Sqlite => {
            let wrap = |mods: Vec<Expr>| {
                let mut args = vec![x.clone()];
                args.extend(mods);
                func(if ts { "DATETIME" } else { "DATE" }, args)
            };
            match unit {
                "YEAR" => wrap(vec![s("start of year")]),
                "MONTH" => wrap(vec![s("start of month")]),
                "DAY" => wrap(vec![s("start of day")]),
                "HOUR" | "MINUTE" => {
                    let fmt = if unit == "HOUR" {
                        "%Y-%m-%d %H:00:00"
                    } else {
                        "%Y-%m-%d %H:%M:00"
                    };
                    func("STRFTIME", vec![s(fmt), x])
                }
                // Back (weekday + 6) % 7 days to Monday (%w is 0 for Sunday).
                "WEEK" => {
                    let dow = cast(func("STRFTIME", vec![s("%w"), x.clone()]), DataType::Int);
                    let back = bin(
                        atomic(bin(dow, BinaryOperator::Plus, n("6"))),
                        BinaryOperator::Modulo,
                        n("7"),
                    );
                    let m = bin(
                        bin(s("-"), BinaryOperator::Concat, atomic(back)),
                        BinaryOperator::Concat,
                        s(" days"),
                    );
                    wrap(vec![s("start of day"), m])
                }
                // Back (month - 1) % 3 months from the start of the month.
                _ => {
                    let month = cast(func("STRFTIME", vec![s("%m"), x.clone()]), DataType::Int);
                    let back = bin(
                        atomic(bin(month, BinaryOperator::Minus, n("1"))),
                        BinaryOperator::Modulo,
                        n("3"),
                    );
                    let m = bin(
                        bin(s("-"), BinaryOperator::Concat, atomic(back)),
                        BinaryOperator::Concat,
                        s(" months"),
                    );
                    wrap(vec![s("start of month"), m])
                }
            }
        }
    })
}

fn part(ts: bool, unit: &str, x: Expr, f: Fam) -> Option<Expr> {
    let ok = match unit {
        "YEAR" | "QUARTER" | "MONTH" | "DAY" | "DOW" | "DOY" => true,
        // A date has no time of day, so these three need a timestamp. `SECOND` was
        // in `field` but not in this list, so `second` was unspellable — the prelude
        // could not define it and `secondsBetween` had nothing to build on.
        "HOUR" | "MINUTE" | "SECOND" => ts,
        _ => false,
    };
    if !ok {
        return None;
    }
    let field = match unit {
        "DOW" => DateTimeField::DayOfWeek,
        "DOY" => DateTimeField::DayOfYear,
        u => field(u)?,
    };
    let extract = |x: Expr| Expr::Extract {
        field: field.clone(),
        expr: Box::new(x),
    };
    Some(match f {
        // Postgres's EXTRACT is numeric; the others are already integers.
        Fam::Ansi => cast(extract(x), DataType::Int),
        Fam::Duck | Fam::Tsql | Fam::Snowflake => extract(x),
        // ISO day of week, 1 = Monday .. 7 = Sunday.
        Fam::Trino if unit == "DOW" => bin(extract(x), BinaryOperator::Modulo, n("7")),
        // 1 = Sunday .. 7 = Saturday.
        Fam::Spark if unit == "DOW" => bin(extract(x), BinaryOperator::Minus, n("1")),
        Fam::Trino | Fam::Spark => extract(x),
        Fam::Mysql => match unit {
            "DOW" => bin(func("DAYOFWEEK", vec![x]), BinaryOperator::Minus, n("1")),
            "DOY" => func("DAYOFYEAR", vec![x]),
            _ => extract(x),
        },
        Fam::BigQuery => match unit {
            "DOW" | "DOY" => {
                let fmt = if unit == "DOW" { "%w" } else { "%j" };
                let fmt_fn = if ts {
                    "FORMAT_TIMESTAMP"
                } else {
                    "FORMAT_DATE"
                };
                cast(func(fmt_fn, vec![s(fmt), x]), DataType::BigInt)
            }
            _ => extract(x),
        },
        Fam::Sqlite => {
            let code = |c: &str| cast(func("STRFTIME", vec![s(c), x.clone()]), DataType::Int);
            // Every unit `ok` above accepts is listed. The previous version ended with
            // `_ =>` for QUARTER, which made the *quarter* formula the answer for any
            // unit that was not spelled out — so `SECOND`, once it became accepted,
            // silently lowered to `(month + 2) / 3`. A catch-all that means one
            // particular case is a trap; this is exhaustive instead, and `ok` is what
            // guarantees the fallback is never reached.
            match unit {
                "YEAR" => code("%Y"),
                "MONTH" => code("%m"),
                "DAY" => code("%d"),
                "DOW" => code("%w"),
                "DOY" => code("%j"),
                "HOUR" => code("%H"),
                "MINUTE" => code("%M"),
                "SECOND" => code("%S"),
                "QUARTER" => bin(
                    atomic(bin(code("%m"), BinaryOperator::Plus, n("2"))),
                    BinaryOperator::Divide,
                    n("3"),
                ),
                _ => return None,
            }
        }
    })
}

/// A `date` argument, spelled so a strict target accepts it.
///
/// A Cagara string literal in a `date` position is typed `date` by the checker
/// and reaches the backend as a string literal, which Postgres, DuckDB, SQLite
/// and MySQL coerce. Trino, BigQuery and Spark do not: `date_diff('day', d,
/// '2024-03-01')` is *rejected* there rather than coerced —
///
/// ```text
/// Unexpected parameters (varchar(3), date, varchar(10)) for function date_diff
/// ```
///
/// — so the literal is cast where the type is known. A column or any other
/// expression is left alone: it already has its type.
fn date_arg(e: Expr, f: Fam) -> Expr {
    let strict = matches!(f, Fam::Trino | Fam::BigQuery | Fam::Spark);
    match e {
        Expr::StringLiteral(_) if strict => cast(e, DataType::Date),
        other => other,
    }
}

fn days_between(a: Expr, b: Expr, f: Fam) -> Expr {
    match f {
        // date - date is an integer number of days.
        Fam::Ansi => bin(atomic(b), BinaryOperator::Minus, atomic(a)),
        Fam::Mysql => func("DATEDIFF", vec![b, a]),
        Fam::Sqlite => {
            let jd = |e: Expr| func("JULIANDAY", vec![e]);
            cast(bin(jd(b), BinaryOperator::Minus, jd(a)), DataType::Int)
        }
        Fam::Duck | Fam::Trino => func("DATE_DIFF", vec![s("day"), a, b]),
        Fam::Spark => func("DATEDIFF", vec![b, a]),
        Fam::Tsql | Fam::Snowflake => func("DATEDIFF", vec![kw("DAY"), a, b]),
        Fam::BigQuery => func("DATE_DIFF", vec![b, a, kw("DAY")]),
    }
}

// ── integer division ───────────────────────────────────────────────────────

/// `a / b` on integers, truncated toward zero (as in Postgres, SQLite,
/// T-SQL, Trino). Elsewhere `/` gives a decimal or float.
/// The divisor, guarded so a zero divisor gives NULL rather than an error.
///
/// The language types `/` and `%` as non-null `int`, and that is the promise the
/// operator makes. Three families already keep it by accident — DuckDB's
/// `DIVIDE`, BigQuery/Spark's `DIV` and SQLite's `/` all yield NULL on zero — but
/// PostgreSQL and Trino **raise**, so the same well-typed program answered
/// differently on different targets. `NULLIF(b, 0)` is what makes the promise
/// true everywhere, which is the least a dialect-independent type can do.
///
/// A target whose own `/` already returns NULL gets no guard: the extra `NULLIF`
/// is dead weight in the plan, and there is a test that the emitted SQL does not
/// carry it.
fn guarded_divisor(b: Expr, f: Fam) -> Expr {
    let already_null = matches!(f, Fam::Sqlite | Fam::Duck | Fam::BigQuery | Fam::Spark);
    if already_null {
        return b;
    }
    func("NULLIF", vec![b, n("0")])
}

fn idiv(a: Expr, b: Expr, f: Fam) -> Expr {
    let b = guarded_divisor(b, f);
    let slash = |a: Expr, b: Expr| bin(atomic(a), BinaryOperator::Divide, atomic(b));
    match f {
        Fam::Ansi | Fam::Sqlite | Fam::Tsql | Fam::Trino => atomic(slash(a, b)),
        Fam::Duck => func("DIVIDE", vec![a, b]),
        Fam::BigQuery => func("DIV", vec![a, b]),
        Fam::Spark => func("DIV", vec![a, b]),
        // `/` rounds to a few decimals here, which can round a quotient up
        // to the next integer; `a - a % b` is an exact multiple of `b`.
        Fam::Mysql | Fam::Snowflake => {
            let rem = bin(atomic(a.clone()), BinaryOperator::Modulo, atomic(b.clone()));
            let exact = slash(bin(atomic(a), BinaryOperator::Minus, atomic(rem)), b);
            // MySQL casts to SIGNED, not BIGINT.
            if f == Fam::Mysql {
                cast(exact, DataType::UserDefined("SIGNED".into()))
            } else {
                func("TRUNC", vec![exact])
            }
        }
    }
}

// ── builders ───────────────────────────────────────────────────────────────

fn field(unit: &str) -> Option<DateTimeField> {
    Some(match unit {
        "YEAR" => DateTimeField::Year,
        "QUARTER" => DateTimeField::Quarter,
        "MONTH" => DateTimeField::Month,
        "WEEK" => DateTimeField::Week,
        "DAY" => DateTimeField::Day,
        "HOUR" => DateTimeField::Hour,
        "MINUTE" => DateTimeField::Minute,
        "SECOND" => DateTimeField::Second,
        _ => return None,
    })
}

/// The base-10 logarithm, on targets that do not spell it `LOG10`.
///
/// `None` where the target has no base-10 logarithm, which is a *refusal* rather than
/// an emitted call: SQL Server has only `LOG(x)` (base `e`) and `LOG(x, base)`, and a
/// template that guessed would produce SQL the engine rejects with a message that does
/// not say what the author wrote.
fn log10(x: Expr, f: Fam) -> Option<Expr> {
    Some(match f {
        // `LOG(x, 10)` is the two-argument form; T-SQL reads it as base-second, so
        // this is exact rather than `LOG(x) / LOG(10)`.
        Fam::Tsql => func("LOG", vec![atomic(x), n("10")]),
        // Every other family has the name: ANSI/Postgres, DuckDB, SQLite, MariaDB,
        // Trino, Spark, BigQuery and Snowflake all read `LOG10`. Listed rather than
        // reached by a catch-all so a new family has to decide.
        Fam::Ansi
        | Fam::Duck
        | Fam::Sqlite
        | Fam::Mysql
        | Fam::Trino
        | Fam::Spark
        | Fam::BigQuery
        | Fam::Snowflake => func("LOG10", vec![x]),
    })
}

/// The base-2 logarithm, on the targets that do not spell it `LOG2`.
///
/// PostgreSQL has only `LOG` (base 10) and `LN`, so the base-2 log is `LN(x) / LN(2)`
/// there — exact for the powers of two these are used on, and the same expression the
/// language uses for `logBase` elsewhere.
fn log2(x: Expr, f: Fam) -> Option<Expr> {
    Some(match f {
        Fam::Ansi => {
            let two = Expr::Number("2.0".into());
            bin(
                func("LN", vec![atomic(x)]),
                BinaryOperator::Divide,
                func("LN", vec![two]),
            )
        }
        Fam::Duck
        | Fam::Sqlite
        | Fam::Mysql
        | Fam::Trino
        | Fam::Spark
        | Fam::BigQuery
        | Fam::Snowflake
        | Fam::Tsql => func("LOG2", vec![x]),
    })
}

/// `repeat n s`, on targets that spell it differently.
///
/// SQLite has no `REPEAT`. `replace(hex(zeroblob(n)), '00', s)` is the portable
/// spelling — `zeroblob(n)` is `n` zero bytes, `hex` makes that `2n` characters of
/// `"00"`, and replacing each with `s` gives `n` copies — measured on all four
/// engines. It is exact rather than approximate, which is why it is used rather than
/// refusing the function on SQLite.
fn repeat(text: Expr, n: Expr, f: Fam) -> Option<Expr> {
    Some(match f {
        Fam::Sqlite => func(
            "REPLACE",
            vec![
                func("HEX", vec![func("ZEROBLOB", vec![atomic(n)])]),
                s("00"),
                atomic(text),
            ],
        ),
        // Trino's `repeat` returns an *array* of copies, not a string — measured:
        // `repeat('a', 3)` is `['a','a','a']`. `array_join` is what makes it the
        // string the language promised, and returning the array would be a type the
        // signature does not allow.
        Fam::Trino => func(
            "ARRAY_JOIN",
            vec![
                func("REPEAT", vec![atomic(text), n]),
                Expr::StringLiteral(String::new()),
            ],
        ),
        // Every other family has `REPEAT` as a string function, in the
        // `(string, count)` order.
        Fam::Ansi
        | Fam::Duck
        | Fam::Mysql
        | Fam::Spark
        | Fam::BigQuery
        | Fam::Snowflake
        | Fam::Tsql => func("REPEAT", vec![atomic(text), n]),
    })
}

/// `reverse s`, on targets that spell it differently.
///
/// SQLite has no `REVERSE` and no portable spelling short of a correlated subquery
/// over the string's characters, so this is a **refusal** there rather than an emitted
/// call: a template that guessed would produce SQL the engine rejects with a message
/// that does not say what the author wrote.
fn reverse(text: Expr, f: Fam) -> Option<Expr> {
    match f {
        Fam::Sqlite => Some(sqlite_reverse(text)),
        // T-SQL spells it `REVERSE` too; `REVERSE` exists on every other family here.
        Fam::Ansi
        | Fam::Duck
        | Fam::Mysql
        | Fam::Trino
        | Fam::Spark
        | Fam::BigQuery
        | Fam::Snowflake
        | Fam::Tsql => Some(func("REVERSE", vec![text])),
    }
}

/// A float in `[0, 1)`, on the targets that spell it that way.
fn random(f: Fam) -> Option<Expr> {
    match f {
        // SQLite's `random()` is a signed 64-bit integer, not a double in `[0, 1)`, so
        // dividing it by 2^63 is negative about half the time — measured. Masking off
        // the sign bit and dividing by the mask's range gives exactly `[0, 1)`.
        Fam::Sqlite => {
            let masked = bin(
                atomic(func("RANDOM", vec![])),
                BinaryOperator::BitwiseAnd,
                n("2147483647"),
            );
            let scaled = cast(masked, float_type(Fam::Sqlite));
            Some(bin(
                atomic(scaled),
                BinaryOperator::Divide,
                n("2147483648.0"),
            ))
        }
        // `RAND()` on MySQL, `RANDOM()` everywhere else; `random()` in DuckDB, Trino,
        // PostgreSQL and SQLite all return a double in `[0, 1)` except SQLite's.
        Fam::Mysql => Some(func("RAND", vec![])),
        Fam::Ansi
        | Fam::Duck
        | Fam::Trino
        | Fam::Spark
        | Fam::BigQuery
        | Fam::Snowflake
        | Fam::Tsql => Some(func("RANDOM", vec![])),
    }
}

fn timestamp() -> DataType {
    DataType::Timestamp {
        precision: None,
        with_tz: false,
    }
}

/// The SQL type that holds an IEEE-754 double, in the target's spelling.
///
/// `FLOAT` is not that type everywhere: it is single precision on MySQL and
/// DuckDB, and T-SQL spells the double as `FLOAT` rather than `DOUBLE`.
fn float_type(f: Fam) -> DataType {
    match f {
        Fam::Ansi => DataType::UserDefined("DOUBLE PRECISION".into()),
        Fam::Mysql | Fam::Sqlite | Fam::Duck | Fam::Snowflake | Fam::Trino | Fam::Spark => {
            DataType::Double
        }
        Fam::Tsql => DataType::Float,
        Fam::BigQuery => DataType::UserDefined("FLOAT64".into()),
    }
}

/// Truncate toward zero, then convert to a 64-bit integer.
///
/// A plain cast is not portable: PostgreSQL and MySQL round when converting a
/// float to an integer, while SQLite and T-SQL truncate. `TRUNC` (or MySQL's
/// `TRUNCATE`) makes the result the same everywhere before the cast.
fn trunc_int(e: Expr, f: Fam) -> Expr {
    match f {
        Fam::Mysql => cast(func("TRUNCATE", vec![e, n("0")]), DataType::BigInt),
        // SQLite and T-SQL cast truncates toward zero already.
        Fam::Sqlite | Fam::Tsql => cast(e, DataType::BigInt),
        _ => cast(func("TRUNC", vec![e]), DataType::BigInt),
    }
}

fn func(name: &str, args: Vec<Expr>) -> Expr {
    Expr::Function {
        name: name.into(),
        args,
        distinct: false,
        filter: None,
        over: None,
        order_by: vec![],
        within_group: false,
    }
}

/// A bare keyword argument such as `DAY` in `DATEADD(DAY, n, x)`.
fn kw(word: &str) -> Expr {
    Expr::Column {
        table: None,
        name: word.into(),
        quote_style: QuoteStyle::None,
        table_quote_style: QuoteStyle::None,
    }
}

fn s(v: &str) -> Expr {
    Expr::StringLiteral(v.into())
}

fn n(v: &str) -> Expr {
    Expr::Number(v.into())
}

fn bin(l: Expr, op: BinaryOperator, r: Expr) -> Expr {
    Expr::BinaryOp {
        left: Box::new(l),
        op,
        right: Box::new(r),
    }
}

fn neg(e: Expr) -> Expr {
    Expr::UnaryOp {
        op: UnaryOperator::Minus,
        expr: Box::new(atomic(e)),
    }
}

fn cast(e: Expr, data_type: DataType) -> Expr {
    Expr::Cast {
        expr: Box::new(atomic(e)),
        data_type,
    }
}

/// `TRY_CAST` for the targets that have it, and the validation emulation for the two
/// that do not.
///
/// The two failures are opposite, which is why this is per target rather than one
/// guard: SQLite's `CAST('abc' AS INT)` is `0` — a silent wrong answer — while
/// PostgreSQL's raises. SQLite is handled by [`sqlite_try_number`] (a
/// `TRIM(x,'0123456789')` test); PostgreSQL by [`pg_try_cast`] (a regex test, which
/// works on every PostgreSQL version, unlike `pg_input_is_valid` from 16).
fn try_cast_for(e: Expr, data_type: DataType, f: Fam) -> Expr {
    match (f, &data_type) {
        (Fam::Sqlite, DataType::BigInt) => sqlite_try_number(e, false),
        (Fam::Sqlite, DataType::Double) => sqlite_try_number(e, true),
        // SQLite's `DATE()`/`DATETIME()` already answer NULL for an unparseable
        // string, which is exactly the contract.
        (Fam::Sqlite, DataType::Date) => func("DATE", vec![e]),
        // PostgreSQL raises on a bad cast, so the guard has to run first.
        (Fam::Ansi, DataType::BigInt) => guarded(e, data_type, INT_SHAPE, f),
        (Fam::Ansi, DataType::Double) => guarded(e, data_type, FLOAT_SHAPE, f),
        // Dates and timestamps keep the older regex guard, whose patterns are the ISO
        // shapes rather than the numeric ones.
        (Fam::Ansi, _) => pg_try_cast(e, data_type),
        // **DuckDB has `TRY_CAST` and still disagrees**, so it is guarded too:
        // `TRY_CAST('1.5' AS BIGINT)` is `2` there — it rounds — and `' 42 '` parses,
        // where the language means "the string is an integer" and neither is one. The
        // guard makes `tryInt` mean one thing on every target; the cast itself stays
        // `TRY_CAST`, so a value the guard somehow lets through still yields NULL.
        (Fam::Duck, DataType::BigInt) => guarded(e, data_type, INT_SHAPE, f),
        (Fam::Duck, DataType::Double) => guarded(e, data_type, FLOAT_SHAPE, f),
        // Every other target has `TRY_CAST` and agrees with the language about what it
        // accepts.
        _ => try_cast(e, data_type),
    }
}

/// The shape an integer string has.
const INT_SHAPE: &str = "^[+-]?[0-9]+$";
/// The shape a float string has. A leading `.` is accepted (`".5"`), which every
/// target's own `tryFloat` also accepts.
const FLOAT_SHAPE: &str = "^[+-]?([0-9]+[.])?[0-9]+$";

/// `TRY_CAST` behind a shape guard, where the engine's own `TRY_CAST` is more
/// permissive than the language.
fn guarded(e: Expr, data_type: DataType, shape: &str, f: Fam) -> Expr {
    let trimmed = func("TRIM", vec![e]);
    let matches = match f {
        // `regexp_matches(s, p)` is DuckDB's boolean regex test.
        Fam::Duck => func("regexp_matches", vec![trimmed.clone(), s(shape)]),
        _ => Expr::TypedFunction {
            func: TypedFunction::RegexpLike {
                expr: Box::new(trimmed.clone()),
                pattern: Box::new(s(shape)),
                flags: None,
            },
            filter: None,
            over: None,
        },
    };
    // The cast sees a value the guard accepted, so it cannot fail; the outer `CASE`
    // discards it when the guard said no.
    let safe = case(matches.clone(), trimmed, s("0"));
    case(matches, cast(safe, data_type), Expr::Null)
}

/// `TRY_CAST(x AS t)` on PostgreSQL, as a regex test beside the cast.
///
/// PostgreSQL has no `TRY_CAST` and its `CAST` raises, so a failed parse has to be
/// prevented rather than caught. `pg_input_is_valid` would be the direct answer but
/// needs version 16, and this project does not require that; a `~` regex works on every
/// version. The three patterns are the shapes a value of that type has, and a NULL
/// input yields NULL through the `CASE` rather than matching:
///
/// ```text
/// CASE WHEN x ~ '^[+-]?[0-9]+$'                 THEN CAST(x AS BIGINT) END  -- int
/// CASE WHEN x ~ '^[+-]?([0-9]+[.])?[0-9]+$'      THEN CAST(x AS DOUBLE) END  -- float
/// CASE WHEN x ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}$'   THEN CAST(x AS DATE) END   -- date
/// ```
///
/// A date the regex accepts can still be impossible (`2024-13-45`); PostgreSQL raises on
/// that, which is `parseDate`'s behaviour too and is the caller's business to avoid.
fn pg_try_cast(e: Expr, data_type: DataType) -> Expr {
    let pattern = match &data_type {
        DataType::BigInt => "^[+-]?[0-9]+$",
        DataType::Date => "^[0-9]{4}-[0-9]{2}-[0-9]{2}$",
        DataType::Timestamp { .. } => "^[0-9]{4}-[0-9]{2}-[0-9]{2}[ T][0-9]{2}:[0-9]{2}:[0-9]{2}$",
        // Everything else reaching here is a float: `DOUBLE PRECISION` is a
        // `UserDefined` name in this crate, so it cannot be matched by variant.
        _ => "^[+-]?([0-9]+[.])?[0-9]+$",
    };
    // `x ~ 'p'` — PostgreSQL's POSIX match, which `RegexpLike` is documented to emit
    // as the operator there.
    let matches = |subject: Expr| Expr::TypedFunction {
        func: TypedFunction::RegexpLike {
            expr: Box::new(subject),
            pattern: Box::new(s(pattern)),
            flags: None,
        },
        filter: None,
        over: None,
    };
    // The cast is applied to *a valid string*, not to `e`: `then CAST(if(match, e,
    // '0'))`. That looks redundant and is not — PostgreSQL constant-folds
    // `CASE WHEN 'abc' ~ … THEN 'abc'::BIGINT END`, raising on the cast during planning
    // before the condition is ever evaluated, so the guarded form still failed for a
    // literal. Substituting a parseable placeholder for the rejected value makes the
    // cast total, and the outer `CASE` then discards it.
    let safe = case(matches(e.clone()), e.clone(), s("0"));
    case(matches(e), cast(safe, data_type), Expr::Null)
}

/// `TRY_CAST`, which yields NULL instead of failing.
///
/// This cannot come from a `sql` template: sqlglot's parser reads
/// `TRY_CAST(x AS INT)` as a plain `Cast`, so the "try" is gone before the
/// backend sees the text and the cast fails on the first bad row. Building the
/// node here is the only way to keep the semantics.
///
/// SQLite and PostgreSQL have no `TRY_CAST`, and both are emulated rather than refused:
/// a failed parse there is fixed per target in [`try_cast_for`], because the two fail
/// in opposite ways — SQLite's `CAST` answers `0` for `'abc'`, and PostgreSQL's raises.
fn try_cast(e: Expr, data_type: DataType) -> Expr {
    Expr::TryCast {
        expr: Box::new(atomic(e)),
        data_type,
    }
}

/// `TRY_CAST`, which SQLite does not have — and its `CAST` never fails, so a failed
/// parse there is a silent `0` rather than NULL.
///
/// The emulation *validates* instead of casting: the string is checked against the
/// spelling a number has, and `CAST` runs only when the check passes, so it is free to
/// be as permissive as it likes because nothing that would go wrong reaches it.
///
/// `TRIM(x, '0123456789')` is the whole trick — it strips a *character set* from both
/// ends, so the result is empty exactly when `x` is all digits. A non-digit anywhere
/// blocks the strip from that side and survives.
///
/// One narrowing, stated rather than hidden: an exponent (`"1e3"`) is not accepted on
/// any dialect, because the check is defined once here and applied to all of them
/// rather than left to each engine's `TRY_CAST`.
fn sqlite_try_number(e: Expr, float: bool) -> Expr {
    let t = func("TRIM", vec![e.clone()]);
    // `b` is `t` without a leading sign: `SUBSTR(t,1,1)` is `-` or `+` for length 1.
    let is_signed = bin(
        atomic(func(
            "INSTR",
            vec![
                s("-+"),
                atomic(func("SUBSTR", vec![atomic(t.clone()), n("1"), n("1")])),
            ],
        )),
        BinaryOperator::Gt,
        n("0"),
    );
    let body = case(
        is_signed,
        func("SUBSTR", vec![atomic(t.clone()), n("2")]),
        t,
    );
    let no_dot = func("REPLACE", vec![body.clone(), s("."), s("")]);
    let dots = bin(
        atomic(func("LENGTH", vec![body.clone()])),
        BinaryOperator::Minus,
        atomic(func("LENGTH", vec![no_dot.clone()])),
    );
    let digits_ok = bin(
        bin(
            atomic(func("TRIM", vec![no_dot.clone(), s("0123456789")])),
            BinaryOperator::Eq,
            s(""),
        ),
        BinaryOperator::And,
        bin(
            atomic(func("LENGTH", vec![no_dot])),
            BinaryOperator::Gt,
            n("0"),
        ),
    );
    let valid = if float {
        bin(
            atomic(digits_ok),
            BinaryOperator::And,
            bin(dots, BinaryOperator::LtEq, n("1")),
        )
    } else {
        bin(
            atomic(digits_ok),
            BinaryOperator::And,
            bin(dots, BinaryOperator::Eq, n("0")),
        )
    };
    case(
        valid,
        cast(
            e,
            if float {
                float_type(Fam::Sqlite)
            } else {
                DataType::BigInt
            },
        ),
        Expr::Null,
    )
}

fn case(cond: Expr, then: Expr, otherwise: Expr) -> Expr {
    Expr::Case {
        operand: None,
        when_clauses: vec![(cond, then)],
        else_clause: Some(Box::new(otherwise)),
    }
}

// ── SQLite's `strftime`, spelled out ─────────────────────────────────────────
//
// `strftime` reads the numeric C specifiers and has no name specifiers: `%B` answers
// NULL rather than failing, so `formatDate "%B"` was a column of NULLs under a
// non-null `string` type. Because the format is a literal the whole call can be taken
// apart instead of refused:
//
//   strftime('%Y-%m', d)  =>  STRFTIME('%Y', d) || '-' || STRFTIME('%m', d)
//   strftime('%B %d', d)  =>  <month-name CASE> || ' ' || STRFTIME('%d', d)
//
// which needs only single-specifier calls — the subset every SQLite has.

/// The width of a numeric field in the *input* string, for the parse offsets.
fn sqlite_field_width(spec: char) -> Option<usize> {
    match spec {
        'Y' => Some(4),
        'j' => Some(3),
        'm' | 'd' | 'H' | 'M' | 'S' => Some(2),
        _ => None,
    }
}

fn month_name(n: u32, abbrev: bool) -> &'static str {
    const FULL: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let name = FULL[(n as usize).clamp(1, 12) - 1];
    if abbrev {
        &name[..3]
    } else {
        name
    }
}

fn day_name(n: u32, abbrev: bool) -> &'static str {
    // `%w` is 0 = Sunday, the order this table is written in.
    const FULL: [&str; 7] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    let name = FULL[(n as usize) % 7];
    if abbrev {
        match name {
            "Tuesday" => "Tue",
            "Thursday" => "Thu",
            "Saturday" => "Sat",
            _ => &name[..3],
        }
    } else {
        name
    }
}

/// `CASE <scrutinee> WHEN '1' THEN … END` over a fixed name table.
fn name_case(scrutinee: Expr, names: impl Fn(u32) -> &'static str, high: u32) -> Expr {
    Expr::Case {
        operand: Some(Box::new(scrutinee)),
        when_clauses: (0..=high)
            .map(|i| {
                (
                    Expr::StringLiteral(i.to_string()),
                    Expr::StringLiteral(names(i).to_string()),
                )
            })
            .collect(),
        else_clause: Some(Box::new(Expr::StringLiteral(String::new()))),
    }
}

/// Concatenate with `||`, which SQLite has had forever (`CONCAT` only from 3.44).
fn concat_all(parts: Vec<Expr>, sep: &str) -> Expr {
    let mut it = parts.into_iter();
    let Some(mut out) = it.next() else {
        return s("");
    };
    for part in it {
        if !sep.is_empty() {
            out = bin(atomic(out), BinaryOperator::Concat, s(sep));
        }
        out = bin(atomic(out), BinaryOperator::Concat, atomic(part));
    }
    out
}

/// `formatDate`/`formatTimestamp` on SQLite.
fn sqlite_format(fmt: &str, value: &Expr, _timestamp: bool) -> Result<Expr, String> {
    let mut parts: Vec<Expr> = Vec::new();
    let mut literal = String::new();
    let mut chars = fmt.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            literal.push(c);
            continue;
        }
        let Some(spec) = chars.next() else {
            return Err(spec_error('%', fmt));
        };
        if spec == '%' {
            literal.push('%');
            continue;
        }
        if !literal.is_empty() {
            parts.push(Expr::StringLiteral(std::mem::take(&mut literal)));
        }
        match spec {
            'Y' | 'm' | 'd' | 'H' | 'M' | 'S' | 'j' => parts.push(func(
                "STRFTIME",
                vec![s(&format!("%{spec}")), value.clone()],
            )),
            'b' | 'B' => {
                let month = func("STRFTIME", vec![s("%m"), value.clone()]);
                parts.push(name_case(
                    cast(month, DataType::Int),
                    |n| month_name(n, spec == 'b'),
                    12,
                ));
            }
            'a' | 'A' => {
                let day = func("STRFTIME", vec![s("%w"), value.clone()]);
                parts.push(name_case(
                    cast(day, DataType::Int),
                    |n| day_name(n, spec == 'a'),
                    6,
                ));
            }
            other => return Err(spec_error(other, fmt)),
        }
    }
    if !literal.is_empty() {
        parts.push(Expr::StringLiteral(literal));
    }
    Ok(concat_all(parts, ""))
}

/// `parseDate`/`parseTimestamp` on SQLite: rebuild the ISO string `DATE()`/`DATETIME()`
/// reads, from fixed-width slices of the input.
///
/// The format is a literal, so each field's position is known at compile time — `%Y%m%d`
/// becomes `SUBSTR(s,1,4) || '-' || SUBSTR(s,5,2) || '-' || SUBSTR(s,7,2)`. A name has
/// no fixed width and is refused, since a name cannot be read back this way.
fn slice_parse(fmt: &str, value: &Expr, with_time: bool, f: Fam) -> Result<Expr, String> {
    let mut offset = 1usize;
    let mut fields: Vec<(char, Expr)> = Vec::new();
    let mut chars = fmt.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            offset += 1;
            continue;
        }
        let Some(spec) = chars.next() else {
            return Err(spec_error('%', fmt));
        };
        if spec == '%' {
            offset += 1;
            continue;
        }
        let Some(width) = sqlite_field_width(spec) else {
            return Err(format!(
                "`%{spec}` in `{fmt}` cannot be read back on this target, which has no \
                 format parser: `parseDate` rebuilds an ISO string from fixed-width \
                 slices of the input, and only the numeric fields have a width. Use \
                 `%Y %m %d %H %M %S %j`, or `toDate` on a string that is already ISO"
            ));
        };
        // Both targets slice 1-based with a length; the name differs.
        fields.push((
            spec,
            func(
                if f == Fam::Tsql {
                    "SUBSTRING"
                } else {
                    "SUBSTR"
                },
                vec![value.clone(), n(&offset.to_string()), n(&width.to_string())],
            ),
        ));
        offset += width;
    }
    let at = |want: char, default: &'static str| {
        fields
            .iter()
            .find(|(c, _)| *c == want)
            .map(|(_, e)| e.clone())
            .unwrap_or_else(|| Expr::StringLiteral(default.to_string()))
    };
    let date = concat_all(vec![at('Y', "1970"), at('m', "01"), at('d', "01")], "-");
    let iso = if with_time {
        let time = concat_all(vec![at('H', "00"), at('M', "00"), at('S', "00")], ":");
        bin(
            atomic(date),
            BinaryOperator::Concat,
            atomic(bin(s(" "), BinaryOperator::Concat, atomic(time))),
        )
    } else {
        date
    };
    // `CAST(iso AS DATE)`: SQLite's `DATE()`/`DATETIME()` and T-SQL's `CAST` both read
    // an ISO string, and both answer NULL/raise on a bad one the way the target's own
    // parser would.
    Ok(match f {
        Fam::Tsql => cast(
            iso,
            if with_time {
                timestamp()
            } else {
                DataType::Date
            },
        ),
        _ => func(if with_time { "DATETIME" } else { "DATE" }, vec![iso]),
    })
}

/// The shared "not a specifier this language translates" message.
fn spec_error(spec: char, fmt: &str) -> String {
    format!(
        "`%{spec}` in `{fmt}` is not a format specifier this language translates; the \
         supported ones are `%Y` (year), `%m` (month), `%d` (day), `%H` (hour), \
         `%M` (minute), `%S` (second), `%b`/`%B` (month name), `%a`/`%A` (weekday \
         name), `%j` (day of year) and `%%` for a percent"
    )
}

// ── `reverse` on SQLite ─────────────────────────────────────────────────────
//
// SQLite has no `REVERSE`, and the usual answer — a recursive CTE — looks like
// something an *expression* cannot carry. It can: a scalar subquery may have its own
// `WITH RECURSIVE`. `reversed(i, acc)` starts at the string's length with an empty
// accumulator and, for each `i` down to 1, appends the character at `i`; the row where
// `i` reached 0 holds the answer.
//
// The loop condition is `i >= 1`, not `i > 1`: the first version used the latter and
// returned `edcb` for `abcde`, because the seed row's `i` is already the length and the
// last step has to run at `i = 1` to append the first character.
/// A `SelectStatement` with every optional clause empty.
///
/// The crate's AST has no `Default` impl, and the reversal below needs three of these,
/// so the field list is written once here rather than three times inline.
fn empty_select() -> SelectStatement {
    SelectStatement {
        comments: Vec::new(),
        ctes: Vec::new(),
        distinct: false,
        top: None,
        columns: Vec::new(),
        from: None,
        joins: Vec::new(),
        where_clause: None,
        group_by: Vec::new(),
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
        fetch_first: None,
        qualify: None,
        window_definitions: Vec::new(),
        query_options: None,
    }
}

fn sqlite_reverse(text: Expr) -> Expr {
    let i = Expr::Column {
        table: None,
        name: "i".into(),
        quote_style: QuoteStyle::None,
        table_quote_style: QuoteStyle::None,
    };
    let acc = Expr::Column {
        table: None,
        name: "acc".into(),
        quote_style: QuoteStyle::None,
        table_quote_style: QuoteStyle::None,
    };
    let from_reversed = FromClause {
        source: TableSource::Table(TableRef {
            catalog: None,
            schema: None,
            name: "reversed".into(),
            alias: None,
            temporal: None,
            name_quote_style: QuoteStyle::None,
            alias_quote_style: QuoteStyle::None,
        }),
    };
    let seed = SelectStatement {
        columns: vec![
            SelectItem::Expr {
                expr: func("CAGARA_LENGTH", vec![text.clone()]),
                alias: Some("i".into()),
                alias_quote_style: QuoteStyle::None,
            },
            SelectItem::Expr {
                expr: s(""),
                alias: Some("acc".into()),
                alias_quote_style: QuoteStyle::None,
            },
        ],
        ..empty_select()
    };
    let step = SelectStatement {
        columns: vec![
            SelectItem::Expr {
                expr: bin(atomic(i.clone()), BinaryOperator::Minus, n("1")),
                alias: None,
                alias_quote_style: QuoteStyle::None,
            },
            SelectItem::Expr {
                expr: bin(
                    atomic(acc.clone()),
                    BinaryOperator::Concat,
                    atomic(func("SUBSTR", vec![text, atomic(i.clone()), n("1")])),
                ),
                alias: None,
                alias_quote_style: QuoteStyle::None,
            },
        ],
        from: Some(from_reversed.clone()),
        where_clause: Some(bin(atomic(i.clone()), BinaryOperator::GtEq, n("1"))),
        ..empty_select()
    };
    let recursion = Statement::SetOperation(SetOperationStatement {
        comments: Vec::new(),
        op: SetOperationType::Union,
        all: true,
        left: Box::new(Statement::Select(seed)),
        right: Box::new(Statement::Select(step)),
        order_by: Vec::new(),
        limit: None,
        offset: None,
        query_options: None,
    });
    let body = SelectStatement {
        ctes: vec![Cte {
            name: "reversed".into(),
            name_quote_style: QuoteStyle::None,
            columns: vec!["i".into(), "acc".into()],
            query: Box::new(recursion),
            materialized: None,
            recursive: true,
        }],
        columns: vec![SelectItem::Expr {
            expr: acc,
            alias: None,
            alias_quote_style: QuoteStyle::None,
        }],
        from: Some(from_reversed),
        where_clause: Some(bin(atomic(i), BinaryOperator::Eq, n("0"))),
        ..empty_select()
    };
    // An empty string has no characters to walk, and `LENGTH('') = 0` would seed
    // `i = 0` and return the empty accumulator anyway — but the subquery is skipped
    // for it rather than executed, which keeps a `'' -> ''` case cheap and obvious.
    Expr::Subquery(Box::new(Statement::Select(body)))
}
