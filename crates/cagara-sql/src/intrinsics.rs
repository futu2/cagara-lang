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
//! | `CAGARA_IDIV(a, b)` / `CAGARA_MOD(a, b)` | integer division truncated toward zero, and its remainder |
//!
//! `kind` is `DATE` or `TIMESTAMP`. Dialects not named below get the ANSI /
//! Postgres spelling.

use crate::dialect::{fam, support, Cap, Fam};
use crate::stage::atomic;
use sqlglot_rust::ast::{
    BinaryOperator, DataType, DateTimeField, Expr, QuoteStyle, TypedFunction, UnaryOperator,
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
        Some(out) => Ok(atomic(out)),
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

/// Why a *known* intrinsic cannot be lowered on this target, when that is the reason.
///
/// `None` from [`intrinsic`] means either "no such intrinsic" or "this target cannot
/// express it", and those are different messages. `reverse` is the first case of the
/// second kind: SQLite has no `REVERSE` and no portable spelling short of a
/// correlated subquery, so the function is refused there rather than emitted. The
/// dialect is not a parameter because the reason is a property of the *function*: no
/// target without `REVERSE` can express it, so nothing here depends on which one asked.
fn refusal(name: &str) -> Option<String> {
    if name == "CAGARA_RANDOM" {
        return Some(
            "`random` is not available on this target: SQLite's `random()` returns a \
             signed 64-bit integer rather than a float in `[0, 1)`, and dividing by 2^63 \
             gives negative values about half the time — measured. There is no portable \
             spelling, so the function is refused rather than given a different type"
                .to_string(),
        );
    }
    if name == "CAGARA_LIST_AT" {
        return Some(
            "`listAt` is available on DuckDB only: PostgreSQL would need `(xs)[i]`, and \
             `sqlglot-rust` 0.11.2 generates `xs[i]` without the parentheses an aggregate \
             needs — `(ARRAY_AGG(id)[2])` is a syntax error there, measured on PGlite. \
             Trino's `element_at` would work, but `list` is not enabled on Trino at all \
             yet"
            .to_string(),
        );
    }
    let what = match name {
        "CAGARA_REVERSE" => "`reverse`",
        _ => return None,
    };
    Some(format!(
        "{what} is not available on this target: it has no `REVERSE`, and no portable \
         spelling short of a subquery over the string's characters. PostgreSQL, DuckDB, \
         MySQL, Trino, Spark, BigQuery, Snowflake and T-SQL have it"
    ))
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
            ('%', _) => "%",
            ('Y', FmtStyle::Strftime) => "%Y",
            ('Y', FmtStyle::Mysql) => "%Y",
            ('Y', FmtStyle::Pg) => "YYYY",
            ('Y', FmtStyle::Java) => "yyyy",
            ('m', FmtStyle::Strftime | FmtStyle::Mysql) => "%m",
            ('m', FmtStyle::Pg) => "MM",
            ('m', FmtStyle::Java) => "MM",
            ('d', FmtStyle::Strftime | FmtStyle::Mysql) => "%d",
            ('d', FmtStyle::Pg) => "DD",
            ('d', FmtStyle::Java) => "dd",
            ('H', FmtStyle::Strftime | FmtStyle::Mysql) => "%H",
            ('H', FmtStyle::Pg) => "HH24",
            ('H', FmtStyle::Java) => "HH",
            ('M', FmtStyle::Strftime) => "%M",
            ('M', FmtStyle::Mysql) => "%i",
            ('M', FmtStyle::Pg) => "MI",
            ('M', FmtStyle::Java) => "mm",
            ('S', FmtStyle::Strftime) => "%S",
            ('S', FmtStyle::Mysql) => "%s",
            ('S', FmtStyle::Pg) => "SS",
            ('S', FmtStyle::Java) => "ss",
            _ => {
                return Err(format!(
                    "`%{spec}` is not a format specifier this language translates; the \
                     supported ones are `%Y` (year), `%m` (month), `%d` (day), `%H` (hour), \
                     `%M` (minute), `%S` (second) and `%%` for a literal percent"
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
        // SQLite formats with `strftime`, so it reads the C-style format even
        // though it has no parser to read it with.
        Fam::Duck | Fam::BigQuery | Fam::Sqlite => FmtStyle::Strftime,
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
            Fam::Sqlite | Fam::Tsql => {
                return Err(
                    "`parseDate` and `parseTimestamp` need a dialect with a format \
                            parser; this one has none (it can still format with the `format*` \
                            pair). The other dialects spell it `TO_DATE`/`TO_TIMESTAMP`, \
                            `STR_TO_DATE`, `strptime`, `date_parse` or `PARSE_DATE`"
                        .to_string(),
                )
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
            Fam::Duck | Fam::Sqlite => func("STRFTIME", vec![value.clone(), f_lit()]),
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
        ("CAGARA_TRY_INT", 1) => try_cast(a[0].clone(), DataType::BigInt),
        ("CAGARA_TRY_FLOAT", 1) => try_cast(a[0].clone(), float_type(f)),
        ("CAGARA_TRY_DATE", 1) => try_cast(a[0].clone(), DataType::Date),
        ("CAGARA_TRY_TIMESTAMP", 1) => try_cast(a[0].clone(), timestamp()),
        ("CAGARA_TRY_DECIMAL", 3) => try_cast(
            a[0].clone(),
            DataType::Decimal {
                precision: num(1),
                scale: num(2),
            },
        ),
        ("CAGARA_LEFT", 2) => match f {
            Fam::Sqlite | Fam::Trino => func("SUBSTR", vec![a[0].clone(), n("1"), a[1].clone()]),
            _ => func("LEFT", a.to_vec()),
        },
        ("CAGARA_RIGHT", 2) => match f {
            // SUBSTR(s, 0) is the whole string, so n = 0 needs a guard.
            Fam::Sqlite => {
                let (x, k) = (a[0].clone(), a[1].clone());
                let tail = func("SUBSTR", vec![x, neg(k.clone()), k.clone()]);
                case(bin(k, BinaryOperator::LtEq, n("0")), s(""), tail)
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
        ("CAGARA_STRPOS", 2) => {
            let (x, sub) = (a[0].clone(), a[1].clone());
            match f {
                Fam::Mysql => func("LOCATE", vec![sub, x]),
                Fam::Sqlite | Fam::Spark => func("INSTR", vec![x, sub]),
                Fam::Tsql | Fam::Snowflake => func("CHARINDEX", vec![sub, x]),
                _ => func("STRPOS", vec![x, sub]),
            }
        }
        ("CAGARA_IDIV", 2) => idiv(a[0].clone(), a[1].clone(), f),
        ("CAGARA_MOD", 2) => match f {
            Fam::BigQuery => func("MOD", a.to_vec()),
            _ => bin(
                atomic(a[0].clone()),
                BinaryOperator::Modulo,
                atomic(a[1].clone()),
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
            | Fam::Trino
            | Fam::Ansi => return None,
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
fn idiv(a: Expr, b: Expr, f: Fam) -> Expr {
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
        Fam::Sqlite => None,
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
///
/// SQLite's `random()` is a *signed 64-bit integer*, not a float — measured, and
/// `random() / 9223372036854775808.0` is negative about half the time, so there is no
/// portable rewrite. Refusing is the honest outcome: emitting `RANDOM()` there would
/// give the column a different type than the signature promises.
fn random(f: Fam) -> Option<Expr> {
    match f {
        Fam::Sqlite => None,
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

/// `TRY_CAST`, which yields NULL instead of failing.
///
/// This cannot come from a `sql` template: sqlglot's parser reads
/// `TRY_CAST(x AS INT)` as a plain `Cast`, so the "try" is gone before the
/// backend sees the text and the cast fails on the first bad row. Building the
/// node here is the only way to keep the semantics.
///
/// SQLite has no `TRY_CAST` and no equivalent, so the node is emitted as-is
/// and the target rejects it. Emulating it is deliberately not attempted:
/// measured, `CAST('abc' AS INT)` is `0` there rather than an error, so a
/// `NULLIF` guard would only catch the empty string and would turn every other
/// unparseable value into a silent `0` -- the exact failure `TRY_CAST` exists
/// to prevent. A loud error at the engine is the honest outcome. See
/// `docs/SQL-DIALECTS.md`, which tracks the per-dialect gaps this way.
fn try_cast(e: Expr, data_type: DataType) -> Expr {
    Expr::TryCast {
        expr: Box::new(atomic(e)),
        data_type,
    }
}

fn case(cond: Expr, then: Expr, otherwise: Expr) -> Expr {
    Expr::Case {
        operand: None,
        when_clauses: vec![(cond, then)],
        else_clause: Some(Box::new(otherwise)),
    }
}
