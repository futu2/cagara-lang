//! A mutable SELECT stage and IR-expression lowering. `sql` templates are
//! parsed by sqlglot with `$n` rewritten to placeholder columns, which are
//! then replaced by the (parenthesized) argument expressions.

use cagara_hir::ir::{self, Bound, Side};
use sqlglot_rust::ast::{
    BinaryOperator, Expr, FromClause, JoinClause, OrderByItem, QuoteStyle, SelectItem,
    SelectStatement, TableSource,
};
use std::cell::RefCell;

pub type Resolver<'a> = dyn Fn(Side, &str) -> Result<Expr, String> + 'a;

/// A mutable SELECT under construction.
///
/// The fields are crate-visible rather than public: `Lowerer` is the only thing
/// that builds one, and a caller outside the crate must not be able to leave a
/// stage in a state the lowering rules do not expect (an aggregate without
/// `has_agg`, a select list whose window flag was not set, …).
pub struct Stage {
    pub(crate) from: TableSource,
    pub(crate) joins: Vec<JoinClause>,
    pub(crate) wheres: Vec<Expr>,
    pub(crate) items: Vec<(String, Expr)>,
    pub(crate) group_by: Vec<Expr>,
    pub(crate) having: Option<Expr>,
    pub(crate) order_by: Vec<OrderByItem>,
    pub(crate) limit: Option<i64>,
    pub(crate) offset: Option<i64>,
    pub(crate) distinct: bool,
    pub(crate) has_agg: bool,
    pub(crate) has_win: bool,
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

/// The clause a lowerer arm is about to fold into a [`Stage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fuse {
    /// `where`: appends to the stage's `WHERE`.
    Filter,
    /// `select` / `update`: replaces the select list.
    Project,
    /// `omit`: drops one name from the select list.
    Omit,
    /// `agg`: sets the grouping.
    Aggregate,
    /// `order`: sets the sort keys.
    Sort,
    Limit,
    Offset,
    Distinct,
}

impl Stage {
    /// May `kind`'s clause be folded into this stage, or must the stage be
    /// wrapped in a derived table first?
    ///
    /// Each clause is legal in the same `SELECT` as some others and not with
    /// the rest, and folding the wrong one changes *which rows* the query
    /// returns rather than producing invalid SQL — so the rule is stated once
    /// here instead of being retyped at each arm:
    ///
    /// | consumer | blocked by |
    /// |---|---|
    /// | filter | aggregate, window, limit, offset |
    /// | project / update | limit, offset, distinct, and (only when the projection adds a window) aggregate or window |
    /// | omit | distinct |
    /// | aggregate | aggregate, window, distinct, limit, offset |
    /// | sort | limit, offset, distinct |
    /// | limit | limit |
    /// | offset | limit, offset |
    /// | distinct | limit, offset |
    ///
    /// `new_window` is the "the projection adds a window" part of the project
    /// rule; the other consumers ignore it.
    pub fn needs_barrier(&self, kind: Fuse, new_window: bool) -> bool {
        let paged = self.limit.is_some() || self.offset.is_some();
        match kind {
            Fuse::Filter => paged || self.has_agg || self.has_win,
            Fuse::Project => {
                paged || self.distinct || (new_window && (self.has_agg || self.has_win))
            }
            Fuse::Omit => self.distinct,
            Fuse::Aggregate => paged || self.distinct || self.has_agg || self.has_win,
            Fuse::Sort => paged || self.distinct,
            Fuse::Limit => self.limit.is_some(),
            Fuse::Offset => paged,
            Fuse::Distinct => paged,
        }
    }

    /// Install a projection's select list.
    ///
    /// `new_window` records that the projection added a window function, which
    /// is what a *later* projection's [`Stage::needs_barrier`] consults. Setting
    /// `items` and forgetting the flag would let two windows fold into one
    /// stage, so the two are written together here.
    pub fn set_items(&mut self, items: Vec<(String, Expr)>, new_window: bool) {
        self.items = items;
        self.has_win |= new_window;
    }

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
            distinct: false,
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
            distinct: self.distinct,
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
    ps.into_iter().map(atomic).reduce(|a, b| Expr::BinaryOp {
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

/// Parenthesize anything that is not already a single term, so a substituted
/// expression keeps the precedence of its slot. A negative number is not a
/// single term: `-$1` of `-5` must not become `--5`, which is a comment.
pub fn atomic(e: Expr) -> Expr {
    match e {
        Expr::Number(ref n) if n.starts_with('-') => Expr::Nested(Box::new(e)),
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
    // Every placeholder must be a whole term naming an argument: `x$1` or
    // `'$1'` would be left in the SQL instead of substituted.
    let n = args.len();
    try_transform_deep(parsed.clone(), &|e| {
        let bad = match &e {
            Expr::Column {
                table: None, name, ..
            } if name.to_ascii_lowercase().contains(ARG) => {
                let k = name.to_ascii_lowercase()[..]
                    .strip_prefix(ARG)
                    .and_then(|k| k.parse::<usize>().ok());
                !k.is_some_and(|k| (1..=n).contains(&k))
            }
            Expr::Column { .. } | Expr::StringLiteral(_) => format!("{e:?}").contains(ARG),
            _ => false,
        };
        if bad {
            Err(format!(
                "SQL template `{sql}`: each `$n` must stand alone (not inside a name or string) \
                 and name one of its {n} argument(s)"
            ))
        } else {
            Ok(e)
        }
    })?;
    let args: Vec<Expr> = args.into_iter().map(atomic).collect();
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
    let failed = RefCell::new(None);
    transform_inner(e, &|e| Ok(f(e)), &failed)
}

/// [`transform_deep`] with a step that can fail.
///
/// `Expr::transform` takes an infallible closure, so a fallible walk has to
/// stop some other way. That is kept inside this function: callers get a
/// `Result` and use `?`, instead of each one opening its own cell to smuggle
/// the message out and inventing a value for the failed node.
pub fn try_transform_deep(
    e: Expr,
    f: &dyn Fn(Expr) -> Result<Expr, String>,
) -> Result<Expr, String> {
    let failed = RefCell::new(None);
    let out = transform_inner(e, f, &failed);
    match failed.take() {
        Some(message) => Err(message),
        None => Ok(out),
    }
}

/// The shared walk. The first error is recorded in `failed`; the node it came
/// from becomes `NULL` (the walk cannot stop early) and the rest of the tree is
/// visited with the step short-circuited, so a caller that sees an error is
/// never handed the partial result.
fn transform_inner(
    e: Expr,
    f: &dyn Fn(Expr) -> Result<Expr, String>,
    failed: &RefCell<Option<String>>,
) -> Expr {
    // Closures rather than free functions so the window-spec descent reuses the
    // same step and error slot as the rest of the walk.
    let deep = |e: Expr| transform_inner(e, f, failed);
    let spec = move |mut s: sqlglot_rust::ast::WindowSpec| {
        s.partition_by = s.partition_by.into_iter().map(deep).collect();
        for o in &mut s.order_by {
            o.expr = deep(std::mem::replace(&mut o.expr, Expr::Null));
        }
        s
    };
    e.transform(&|e| {
        if failed.borrow().is_some() {
            return Expr::Null;
        }
        let e = match e {
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
        };
        match f(e) {
            Ok(e) => e,
            Err(message) => {
                let mut slot = failed.borrow_mut();
                if slot.is_none() {
                    *slot = Some(message);
                }
                Expr::Null
            }
        }
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
                // `template` rejected every placeholder that is not a whole
                // term naming one of its arguments before this runs, so an
                // unresolvable index is unreachable. Asserting keeps it from
                // silently emitting `cagara_arg_N` as SQL if that check ever
                // loosens.
                None => {
                    debug_assert!(false, "unvalidated placeholder `{name}`");
                    e
                }
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
        ir::Expr::In(value, list, negated) => Ok(Expr::InList {
            expr: Box::new(lower_expr(value, r)?),
            list: list
                .iter()
                .map(|item| lower_expr(item, r))
                .collect::<Result<_, _>>()?,
            negated: *negated,
        }),
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
                            if *asc {
                                "ASC NULLS LAST"
                            } else {
                                "DESC NULLS LAST"
                            }
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
