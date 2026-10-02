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
//! | `CAGARA_LEFT(s, n)` / `CAGARA_RIGHT(s, n)` | first / last `n` characters        |
//! | `CAGARA_STRPOS(s, sub)`             | 1-based position of `sub` in `s`, 0 if absent |
//! | `CAGARA_LENGTH(s)`                  | length in characters (trailing spaces count) |
//! | `CAGARA_IDIV(a, b)` / `CAGARA_MOD(a, b)` | integer division truncated toward zero, and its remainder |
//!
//! `kind` is `DATE` or `TIMESTAMP`. Dialects not named below get the ANSI /
//! Postgres spelling.

use crate::stage::atomic;
use sqlglot_rust::ast::{
    BinaryOperator, DataType, DateTimeField, Expr, QuoteStyle, TypedFunction, UnaryOperator,
};
use sqlglot_rust::Dialect;
use std::cell::Cell;

#[derive(Clone, Copy, PartialEq)]
enum Fam {
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

fn fam(d: Dialect) -> Fam {
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

/// Lower every intrinsic in `e` (bottom-up, so arguments are lowered first).
/// A `CAGARA_*` call that is not a known intrinsic is an error rather than
/// SQL that the target engine would reject.
pub fn lower(e: Expr, to: Dialect) -> Result<Expr, String> {
    let f = fam(to);
    let failed = Cell::new(None);
    let out = crate::stage::transform_deep(e, &|e| match node(e, f) {
        Ok(e) => e,
        Err(m) => {
            failed.set(failed.take().or(Some(m)));
            Expr::Null
        }
    });
    match failed.into_inner() {
        Some(m) => Err(m),
        None => Ok(out),
    }
}

fn node(e: Expr, f: Fam) -> Result<Expr, String> {
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
    let lowered = if over.is_none() {
        intrinsic(&upper, &args, f)
    } else {
        None
    };
    match lowered {
        // Non-atomic results are parenthesized so they keep precedence
        // inside whatever template they were substituted into.
        Some(out) => Ok(atomic(out)),
        None => Err(format!(
            "unknown SQL intrinsic `{upper}` with {} argument(s)",
            args.len()
        )),
    }
}

fn intrinsic(name: &str, a: &[Expr], f: Fam) -> Option<Expr> {
    let lit = |i: usize| match a.get(i) {
        Some(Expr::StringLiteral(s)) => Some(s.to_ascii_uppercase()),
        _ => None,
    };
    let ts = |i: usize| lit(i).map(|k| k == "TIMESTAMP");
    Some(match (name, a.len()) {
        ("CAGARA_ADD", 4) => add(ts(0)?, &lit(1)?, a[2].clone(), a[3].clone(), f)?,
        ("CAGARA_TRUNC", 3) => trunc(ts(0)?, &lit(1)?, a[2].clone(), f)?,
        ("CAGARA_PART", 3) => part(ts(0)?, &lit(1)?, a[2].clone(), f)?,
        ("CAGARA_DAYS_BETWEEN", 2) => days_between(a[0].clone(), a[1].clone(), f),
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
        "HOUR" | "MINUTE" => ts,
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
            match unit {
                "YEAR" => code("%Y"),
                "MONTH" => code("%m"),
                "DAY" => code("%d"),
                "DOW" => code("%w"),
                "DOY" => code("%j"),
                "HOUR" => code("%H"),
                "MINUTE" => code("%M"),
                // (month + 2) / 3
                _ => bin(
                    atomic(bin(code("%m"), BinaryOperator::Plus, n("2"))),
                    BinaryOperator::Divide,
                    n("3"),
                ),
            }
        }
    })
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

fn timestamp() -> DataType {
    DataType::Timestamp {
        precision: None,
        with_tz: false,
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

fn case(cond: Expr, then: Expr, otherwise: Expr) -> Expr {
    Expr::Case {
        operand: None,
        when_clauses: vec![(cond, then)],
        else_clause: Some(Box::new(otherwise)),
    }
}
