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
//! | `CAGARA_LENGTH(s)`                  | length in characters                      |
//!
//! `kind` is `DATE` or `TIMESTAMP`. Dialects not named below get the ANSI /
//! Postgres spelling.

use sqlglot_rust::ast::{BinaryOperator, DataType, DateTimeField, Expr, QuoteStyle, TypedFunction, UnaryOperator};
use sqlglot_rust::Dialect;

#[derive(Clone, Copy, PartialEq)]
enum Fam {
    Ansi,
    Mysql,
    Sqlite,
    Duck,
    Tsql,
    BigQuery,
    Snowflake,
}

fn fam(d: Dialect) -> Fam {
    match d {
        Dialect::Mysql | Dialect::Doris | Dialect::SingleStore | Dialect::StarRocks => Fam::Mysql,
        Dialect::Sqlite => Fam::Sqlite,
        Dialect::DuckDb => Fam::Duck,
        Dialect::Tsql | Dialect::Fabric => Fam::Tsql,
        Dialect::BigQuery => Fam::BigQuery,
        Dialect::Snowflake => Fam::Snowflake,
        _ => Fam::Ansi,
    }
}

/// Lower every intrinsic in `e` (bottom-up, so arguments are lowered first).
pub fn lower(e: Expr, to: Dialect) -> Expr {
    let f = fam(to);
    e.transform(&|e| node(e, f))
}

fn node(e: Expr, f: Fam) -> Expr {
    let Expr::Function { name, args, distinct, filter, over, order_by, within_group } = e else { return e };
    let lowered = if name.to_ascii_uppercase().starts_with("CAGARA_") && over.is_none() {
        intrinsic(&name.to_ascii_uppercase(), &args, f)
    } else {
        None
    };
    match lowered {
        // Non-atomic results are parenthesized so they keep precedence
        // inside whatever template they were substituted into.
        Some(out) => atomic(out),
        None => Expr::Function { name, args, distinct, filter, over, order_by, within_group },
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
            _ => Expr::TypedFunction { func: TypedFunction::CurrentTimestamp, filter: None, over: None },
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
            _ => cast(a[0].clone(), DataType::Text),
        },
        ("CAGARA_LEFT", 2) => match f {
            Fam::Sqlite => func("SUBSTR", vec![a[0].clone(), n("1"), a[1].clone()]),
            _ => func("LEFT", a.to_vec()),
        },
        ("CAGARA_RIGHT", 2) => match f {
            // SUBSTR(s, 0) is the whole string, so n = 0 needs a guard.
            Fam::Sqlite => {
                let (x, k) = (a[0].clone(), a[1].clone());
                let tail = func("SUBSTR", vec![x, neg(k.clone()), k.clone()]);
                case(bin(k, BinaryOperator::LtEq, n("0")), s(""), tail)
            }
            _ => func("RIGHT", a.to_vec()),
        },
        // sqlglot spells LENGTH as LEN for BigQuery (which has no LEN); MySQL's
        // LENGTH counts bytes.
        ("CAGARA_LENGTH", 1) => match f {
            Fam::Mysql => func("CHAR_LENGTH", a.to_vec()),
            Fam::Tsql => func("LEN", a.to_vec()),
            _ => func("LENGTH", a.to_vec()),
        },
        ("CAGARA_STRPOS", 2) => {
            let (x, sub) = (a[0].clone(), a[1].clone());
            match f {
                Fam::Mysql => func("LOCATE", vec![sub, x]),
                Fam::Sqlite => func("INSTR", vec![x, sub]),
                Fam::Tsql | Fam::Snowflake => func("CHARINDEX", vec![sub, x]),
                _ => func("STRPOS", vec![x, sub]),
            }
        }
        _ => return None,
    })
}

// ── date arithmetic ────────────────────────────────────────────────────────

fn add(ts: bool, unit: &str, x: Expr, count: Expr, f: Fam) -> Option<Expr> {
    // Weeks and quarters are days and months, which every dialect has.
    let (unit, count) = match unit {
        "WEEK" => ("DAY", bin(atomic(count), BinaryOperator::Multiply, n("7"))),
        "QUARTER" => ("MONTH", bin(atomic(count), BinaryOperator::Multiply, n("3"))),
        u => (u, count),
    };
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
            bin(count, BinaryOperator::Multiply, Expr::Interval { value: Box::new(s("1")), unit: Some(field) }),
        )),
        Fam::Mysql => func("DATE_ADD", vec![x, Expr::Interval { value: Box::new(count), unit: Some(field) }]),
        Fam::Sqlite => {
            let plural = format!(" {}s", unit.to_ascii_lowercase());
            let modifier = bin(count, BinaryOperator::Concat, s(&plural));
            func(if ts { "DATETIME" } else { "DATE" }, vec![x, modifier])
        }
        Fam::Duck => {
            let to = format!("TO_{unit}S");
            same(bin(atomic(x), BinaryOperator::Plus, func(&to, vec![count])))
        }
        Fam::Tsql | Fam::Snowflake => func("DATEADD", vec![kw(unit), count, x]),
        Fam::BigQuery => {
            let iv = Expr::Interval { value: Box::new(count), unit: Some(field) };
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
        Fam::Snowflake => func("DATE_TRUNC", vec![s(unit), x]),
        Fam::Tsql => func("DATETRUNC", vec![kw(if unit == "WEEK" { "ISO_WEEK" } else { unit }), x]),
        Fam::BigQuery => {
            let u = kw(if unit == "WEEK" { "ISOWEEK" } else { unit });
            func(if ts { "TIMESTAMP_TRUNC" } else { "DATE_TRUNC" }, vec![x, u])
        }
        Fam::Mysql => {
            let back = |e: Expr| cast(e, if ts { DataType::DateTime } else { DataType::Date });
            match unit {
                // Monday of the ISO week: WEEKDAY is 0 for Monday.
                "WEEK" => back(func(
                    "DATE_SUB",
                    vec![func("DATE", vec![x.clone()]), Expr::Interval {
                        value: Box::new(func("WEEKDAY", vec![x])),
                        unit: Some(DateTimeField::Day),
                    }],
                )),
                "QUARTER" => {
                    let jan1 = func("MAKEDATE", vec![func("YEAR", vec![x.clone()]), n("1")]);
                    let months = bin(
                        atomic(bin(func("QUARTER", vec![x]), BinaryOperator::Minus, n("1"))),
                        BinaryOperator::Multiply,
                        n("3"),
                    );
                    back(func("DATE_ADD", vec![jan1, Expr::Interval { value: Box::new(atomic(months)), unit: Some(DateTimeField::Month) }]))
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
                    let fmt = if unit == "HOUR" { "%Y-%m-%d %H:00:00" } else { "%Y-%m-%d %H:%M:00" };
                    func("STRFTIME", vec![s(fmt), x])
                }
                // Back (weekday + 6) % 7 days to Monday (%w is 0 for Sunday).
                "WEEK" => {
                    let dow = cast(func("STRFTIME", vec![s("%w"), x.clone()]), DataType::Int);
                    let back = bin(atomic(bin(dow, BinaryOperator::Plus, n("6"))), BinaryOperator::Modulo, n("7"));
                    let m = bin(bin(s("-"), BinaryOperator::Concat, atomic(back)), BinaryOperator::Concat, s(" days"));
                    wrap(vec![s("start of day"), m])
                }
                // Back (month - 1) % 3 months from the start of the month.
                _ => {
                    let month = cast(func("STRFTIME", vec![s("%m"), x.clone()]), DataType::Int);
                    let back = bin(atomic(bin(month, BinaryOperator::Minus, n("1"))), BinaryOperator::Modulo, n("3"));
                    let m = bin(bin(s("-"), BinaryOperator::Concat, atomic(back)), BinaryOperator::Concat, s(" months"));
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
    let extract = |x: Expr| Expr::Extract { field: field.clone(), expr: Box::new(x) };
    Some(match f {
        // Postgres's EXTRACT is numeric; the others are already integers.
        Fam::Ansi => cast(extract(x), DataType::Int),
        Fam::Duck | Fam::Tsql | Fam::Snowflake => extract(x),
        Fam::Mysql => match unit {
            "DOW" => bin(func("DAYOFWEEK", vec![x]), BinaryOperator::Minus, n("1")),
            "DOY" => func("DAYOFYEAR", vec![x]),
            _ => extract(x),
        },
        Fam::BigQuery => match unit {
            "DOW" | "DOY" => {
                let fmt = if unit == "DOW" { "%w" } else { "%j" };
                let fmt_fn = if ts { "FORMAT_TIMESTAMP" } else { "FORMAT_DATE" };
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
                _ => bin(atomic(bin(code("%m"), BinaryOperator::Plus, n("2"))), BinaryOperator::Divide, n("3")),
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
        Fam::Duck => func("DATE_DIFF", vec![s("day"), a, b]),
        Fam::Tsql | Fam::Snowflake => func("DATEDIFF", vec![kw("DAY"), a, b]),
        Fam::BigQuery => func("DATE_DIFF", vec![b, a, kw("DAY")]),
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
    DataType::Timestamp { precision: None, with_tz: false }
}

fn func(name: &str, args: Vec<Expr>) -> Expr {
    Expr::Function { name: name.into(), args, distinct: false, filter: None, over: None, order_by: vec![], within_group: false }
}

/// A bare keyword argument such as `DAY` in `DATEADD(DAY, n, x)`.
fn kw(word: &str) -> Expr {
    Expr::Column { table: None, name: word.into(), quote_style: QuoteStyle::None, table_quote_style: QuoteStyle::None }
}

fn s(v: &str) -> Expr {
    Expr::StringLiteral(v.into())
}

fn n(v: &str) -> Expr {
    Expr::Number(v.into())
}

fn bin(l: Expr, op: BinaryOperator, r: Expr) -> Expr {
    Expr::BinaryOp { left: Box::new(l), op, right: Box::new(r) }
}

fn neg(e: Expr) -> Expr {
    Expr::UnaryOp { op: UnaryOperator::Minus, expr: Box::new(atomic(e)) }
}

fn cast(e: Expr, data_type: DataType) -> Expr {
    Expr::Cast { expr: Box::new(atomic(e)), data_type }
}

fn case(cond: Expr, then: Expr, otherwise: Expr) -> Expr {
    Expr::Case { operand: None, when_clauses: vec![(cond, then)], else_clause: Some(Box::new(otherwise)) }
}

/// Parenthesize anything that is not already a single term.
fn atomic(e: Expr) -> Expr {
    match e {
        Expr::Column { .. }
        | Expr::Number(_)
        | Expr::StringLiteral(_)
        | Expr::Boolean(_)
        | Expr::Null
        | Expr::Function { .. }
        | Expr::TypedFunction { .. }
        | Expr::Cast { .. }
        | Expr::Extract { .. }
        | Expr::Case { .. }
        | Expr::Nested(_) => e,
        other => Expr::Nested(Box::new(other)),
    }
}
