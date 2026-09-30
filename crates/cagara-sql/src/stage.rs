//! A mutable SELECT stage and IR-expression lowering. `sql` templates are
//! parsed by sqlglot with `$n` rewritten to placeholder columns, which are
//! then replaced by the (parenthesized) argument expressions.

use cagara_hir::ir::{self, Bound, Side};
use sqlglot_rust::ast::{
    BinaryOperator, Expr, FromClause, JoinClause, OrderByItem, QuoteStyle, SelectItem,
    SelectStatement, TableSource,
};

pub type Resolver<'a> = dyn Fn(Side, &str) -> Result<Expr, String> + 'a;

pub struct Stage {
    pub from: TableSource,
    pub joins: Vec<JoinClause>,
    pub wheres: Vec<Expr>,
    pub items: Vec<(String, Expr)>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub order_by: Vec<OrderByItem>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub has_agg: bool,
    pub has_win: bool,
}

pub fn col(table: Option<&str>, name: &str) -> Expr {
    Expr::Column {
        table: table.map(str::to_string),
        name: name.to_string(),
        quote_style: ident_style(name),
        table_quote_style: QuoteStyle::None,
    }
}

/// Words that cannot be a bare identifier in some supported dialect. Quoting
/// one needlessly is harmless, so this errs on the side of too many.
const RESERVED: &[&str] = &[
    "all",
    "alter",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "asymmetric",
    "authorization",
    "between",
    "binary",
    "both",
    "by",
    "case",
    "cast",
    "check",
    "collate",
    "column",
    "constraint",
    "create",
    "cross",
    "cube",
    "current",
    "current_date",
    "current_time",
    "current_timestamp",
    "current_user",
    "database",
    "default",
    "delete",
    "desc",
    "distinct",
    "div",
    "do",
    "drop",
    "else",
    "end",
    "except",
    "exists",
    "false",
    "fetch",
    "filter",
    "for",
    "foreign",
    "from",
    "full",
    "grant",
    "group",
    "grouping",
    "having",
    "if",
    "ilike",
    "in",
    "index",
    "inner",
    "insert",
    "intersect",
    "interval",
    "into",
    "is",
    "join",
    "key",
    "keys",
    "lateral",
    "leading",
    "left",
    "like",
    "limit",
    "localtime",
    "localtimestamp",
    "minus",
    "mod",
    "natural",
    "not",
    "null",
    "of",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "outer",
    "over",
    "partition",
    "primary",
    "qualify",
    "range",
    "references",
    "regexp",
    "returning",
    "right",
    "rlike",
    "rollup",
    "row",
    "rows",
    "sample",
    "schema",
    "select",
    "session_user",
    "set",
    "similar",
    "some",
    "symmetric",
    "table",
    "tablesample",
    "then",
    "to",
    "top",
    "trailing",
    "true",
    "union",
    "unique",
    "update",
    "user",
    "using",
    "values",
    "view",
    "when",
    "where",
    "window",
    "with",
];

/// Whether `name` must be quoted to be read back as the same identifier:
/// anything but a lowercase ASCII word that is not reserved. Quoting also
/// keeps case (`userId` would otherwise fold) and escapes the quote char.
pub fn needs_quotes(name: &str) -> bool {
    let mut cs = name.chars();
    let plain = matches!(cs.next(), Some('a'..='z' | '_'))
        && cs.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_'));
    !plain || RESERVED.contains(&name)
}

/// Quote style for an identifier; the generator maps a quoted one to the
/// target dialect's quotes (`"x"`, `` `x` ``, `[x]`).
pub fn ident_style(name: &str) -> QuoteStyle {
    if needs_quotes(name) {
        QuoteStyle::DoubleQuote
    } else {
        QuoteStyle::None
    }
}

impl Stage {
    pub fn new(from: TableSource, items: Vec<(String, Expr)>) -> Self {
        Stage {
            from,
            joins: vec![],
            wheres: vec![],
            items,
            group_by: vec![],
            having: None,
            order_by: vec![],
            limit: None,
            offset: None,
            has_agg: false,
            has_win: false,
        }
    }

    pub fn names(&self) -> Vec<String> {
        self.items.iter().map(|(n, _)| n.clone()).collect()
    }

    pub fn item(&self, n: &str) -> Result<Expr, String> {
        self.items
            .iter()
            .find(|(k, _)| k == n)
            .map(|(_, e)| e.clone())
            .ok_or_else(|| format!("internal: stage has no column `{n}`"))
    }

    /// Lower an IR expression whose columns refer to this stage's outputs.
    pub fn resolve(&self, e: &ir::Expr) -> Result<Expr, String> {
        lower_expr(e, &|side, n| match side {
            Side::Single => self.item(n),
            _ => Err(format!(
                "`.<{n}` / `.>{n}` can only be used in a join predicate"
            )),
        })
    }

    /// A plain `SELECT a, b FROM t` that can be used as a table reference.
    pub fn is_bare_table(&self) -> bool {
        matches!(self.from, TableSource::Table(_))
            && self.joins.is_empty()
            && self.wheres.is_empty()
            && self.group_by.is_empty()
            && self.having.is_none()
            && self.order_by.is_empty()
            && self.limit.is_none()
            && self.offset.is_none()
            && !self.has_agg
            && !self.has_win
            && self
                .items
                .iter()
                .all(|(n, e)| matches!(e, Expr::Column { table: None, name, .. } if name == n))
    }

    pub fn into_statement(self) -> SelectStatement {
        let where_clause = and_all(self.wheres);
        let columns = self
            .items
            .into_iter()
            .map(|(n, e)| {
                let same = matches!(&e, Expr::Column { name, .. } if *name == n);
                SelectItem::Expr {
                    expr: e,
                    alias_quote_style: ident_style(&n),
                    alias: (!same).then_some(n),
                }
            })
            .collect();
        SelectStatement {
            comments: vec![],
            ctes: vec![],
            distinct: false,
            top: None,
            columns,
            from: Some(FromClause { source: self.from }),
            joins: self.joins,
            where_clause,
            group_by: self.group_by,
            having: self.having,
            order_by: self.order_by,
            limit: self.limit.map(|n| Expr::Number(n.to_string())),
            offset: self.offset.map(|n| Expr::Number(n.to_string())),
            fetch_first: None,
            qualify: None,
            window_definitions: vec![],
            query_options: None,
        }
    }
}

/// Conjunction of predicates (each parenthesized), or `None` if empty.
pub fn and_all(ps: impl IntoIterator<Item = Expr>) -> Option<Expr> {
    ps.into_iter().map(paren).reduce(|a, b| Expr::BinaryOp {
        left: Box::new(a),
        op: BinaryOperator::And,
        right: Box::new(b),
    })
}

/// Qualify the unqualified columns of `e` with a table alias.
pub fn qualify(e: Expr, alias: &str) -> Expr {
    e.transform(&|e| match e {
        Expr::Column {
            table: None,
            name,
            quote_style,
            table_quote_style,
        } => Expr::Column {
            table: Some(alias.to_string()),
            name,
            quote_style,
            table_quote_style,
        },
        other => other,
    })
}

/// Parenthesize compound expressions so template substitution keeps precedence.
/// A negative number is compound too: `-$1` of `-5` must not become `--5`,
/// which is a comment.
fn paren(e: Expr) -> Expr {
    match e {
        Expr::Number(ref n) if n.starts_with('-') => Expr::Nested(Box::new(e)),
        Expr::Column { .. }
        | Expr::Number(_)
        | Expr::StringLiteral(_)
        | Expr::Boolean(_)
        | Expr::Null
        | Expr::Function { .. }
        | Expr::TypedFunction { .. }
        | Expr::Nested(_)
        | Expr::Star => e,
        other => Expr::Nested(Box::new(other)),
    }
}

const ARG: &str = "cagara_arg_";

pub fn template(sql: &str, args: Vec<Expr>) -> Result<Expr, String> {
    let b = sql.as_bytes();
    let mut text = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'$' && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
            let j = (i + 1..b.len())
                .find(|&j| !b[j].is_ascii_digit())
                .unwrap_or(b.len());
            text.push_str(ARG);
            text.push_str(&sql[i + 1..j]);
            i = j;
        } else {
            let c = sql[i..].chars().next().unwrap();
            text.push(c);
            i += c.len_utf8();
        }
    }
    let parsed = sqlglot_rust::parse_expr(&text)
        .ok_or_else(|| format!("cannot parse SQL template `{sql}`"))?;
    let args: Vec<Expr> = args.into_iter().map(paren).collect();
    Ok(subst(parsed, &args))
}

/// Replace placeholder columns with arguments. `Expr::transform` does not
/// descend into a function's OVER clause, so window specs are handled here.
fn subst(e: Expr, args: &[Expr]) -> Expr {
    e.transform(&|e| subst_node(e, args))
}

/// `Expr::transform` (bottom-up) that also rewrites the expressions of
/// window specs, which `transform` skips.
pub fn transform_deep(e: Expr, f: &dyn Fn(Expr) -> Expr) -> Expr {
    let spec = |mut s: sqlglot_rust::ast::WindowSpec| {
        s.partition_by = s
            .partition_by
            .into_iter()
            .map(|p| transform_deep(p, f))
            .collect();
        for o in &mut s.order_by {
            o.expr = transform_deep(std::mem::replace(&mut o.expr, Expr::Null), f);
        }
        s
    };
    e.transform(&|e| {
        f(match e {
            Expr::Function {
                name,
                args,
                distinct,
                filter,
                over: Some(s),
                order_by,
                within_group,
            } => Expr::Function {
                name,
                args,
                distinct,
                filter,
                over: Some(spec(s)),
                order_by,
                within_group,
            },
            Expr::TypedFunction {
                func,
                filter,
                over: Some(s),
            } => Expr::TypedFunction {
                func,
                filter,
                over: Some(spec(s)),
            },
            other => other,
        })
    })
}

fn subst_node(e: Expr, args: &[Expr]) -> Expr {
    match e {
        Expr::Column {
            table: None,
            ref name,
            ..
        } if name.to_ascii_lowercase().starts_with(ARG) => {
            match name[ARG.len()..]
                .parse::<usize>()
                .ok()
                .and_then(|n| n.checked_sub(1))
                .and_then(|n| args.get(n))
            {
                Some(a) => a.clone(),
                None => e,
            }
        }
        Expr::Function {
            name,
            args: fargs,
            distinct,
            filter,
            over: Some(spec),
            order_by,
            within_group,
        } => {
            let spec = subst_spec(spec, args);
            Expr::Function {
                name,
                args: fargs,
                distinct,
                filter,
                over: Some(spec),
                order_by,
                within_group,
            }
        }
        // Ranking functions such as ROW_NUMBER() parse as typed functions.
        Expr::TypedFunction {
            func,
            filter,
            over: Some(spec),
        } => Expr::TypedFunction {
            func,
            filter,
            over: Some(subst_spec(spec, args)),
        },
        other => other,
    }
}

fn subst_spec(
    mut spec: sqlglot_rust::ast::WindowSpec,
    args: &[Expr],
) -> sqlglot_rust::ast::WindowSpec {
    spec.partition_by = spec
        .partition_by
        .into_iter()
        .map(|p| subst(p, args))
        .collect();
    for o in &mut spec.order_by {
        o.expr = subst(std::mem::replace(&mut o.expr, Expr::Null), args);
    }
    spec
}

fn lit(l: &ir::Lit) -> Expr {
    match l {
        ir::Lit::Int(i) => Expr::Number(i.to_string()),
        ir::Lit::Float(f) => Expr::Number(f.clone()),
        ir::Lit::Str(s) => Expr::StringLiteral(s.clone()),
        ir::Lit::Bool(b) => Expr::Boolean(*b),
    }
}

fn bound(b: Bound) -> String {
    match b {
        Bound::UnboundedPreceding => "UNBOUNDED PRECEDING".into(),
        Bound::Preceding(n) => format!("{n} PRECEDING"),
        Bound::CurrentRow => "CURRENT ROW".into(),
        Bound::Following(n) => format!("{n} FOLLOWING"),
        Bound::UnboundedFollowing => "UNBOUNDED FOLLOWING".into(),
    }
}

pub fn lower_expr(e: &ir::Expr, r: &Resolver) -> Result<Expr, String> {
    let all = |xs: &[ir::Expr]| {
        xs.iter()
            .map(|x| lower_expr(x, r))
            .collect::<Result<Vec<_>, _>>()
    };
    match e {
        ir::Expr::Col(side, n) => r(*side, n),
        ir::Expr::Lit(l) => Ok(lit(l)),
        ir::Expr::Tpl(sql, args) | ir::Expr::Agg(sql, args) => template(sql, all(args)?),
        ir::Expr::Group(k) => lower_expr(k, r),
        ir::Expr::Win(sql, args, spec) => {
            let mut args = all(args)?;
            let place = |x: Expr, args: &mut Vec<Expr>| {
                args.push(x);
                format!("${}", args.len())
            };
            let mut over = Vec::new();
            if !spec.partition.is_empty() {
                let ps: Vec<String> = spec
                    .partition
                    .iter()
                    .map(|p| Ok(place(lower_expr(p, r)?, &mut args)))
                    .collect::<Result<_, String>>()?;
                over.push(format!("PARTITION BY {}", ps.join(", ")));
            }
            if !spec.order.is_empty() {
                let os: Vec<String> = spec
                    .order
                    .iter()
                    .map(|(k, asc)| {
                        Ok(format!(
                            "{} {}",
                            place(lower_expr(k, r)?, &mut args),
                            if *asc { "ASC" } else { "DESC" }
                        ))
                    })
                    .collect::<Result<_, String>>()?;
                over.push(format!("ORDER BY {}", os.join(", ")));
            }
            if let Some(f) = spec.frame {
                over.push(format!(
                    "ROWS BETWEEN {} AND {}",
                    bound(f.start),
                    bound(f.end)
                ));
            }
            template(&format!("{sql} OVER ({})", over.join(" ")), args)
        }
    }
}
