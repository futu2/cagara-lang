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
        quote_style: QuoteStyle::None,
        table_quote_style: QuoteStyle::None,
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
            _ => Err(format!("`.<{n}` / `.>{n}` can only be used in a join predicate")),
        })
    }

    /// A plain `SELECT a, b FROM t` that can be used as a table reference.
    pub fn is_bare_table(&self) -> bool {
        matches!(self.from, TableSource::Table(_))
            && self.joins.is_empty()
            && self.wheres.is_empty()
            && self.group_by.is_empty()
            && self.order_by.is_empty()
            && self.limit.is_none()
            && self.offset.is_none()
            && !self.has_agg
            && !self.has_win
            && self.items.iter().all(|(n, e)| matches!(e, Expr::Column { table: None, name, .. } if name == n))
    }

    pub fn into_statement(self) -> SelectStatement {
        let where_clause = self.wheres.into_iter().map(paren).reduce(|a, b| Expr::BinaryOp {
            left: Box::new(a),
            op: BinaryOperator::And,
            right: Box::new(b),
        });
        let columns = self
            .items
            .into_iter()
            .map(|(n, e)| {
                let same = matches!(&e, Expr::Column { name, .. } if *name == n);
                SelectItem::Expr { expr: e, alias: (!same).then_some(n), alias_quote_style: QuoteStyle::None }
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
            having: None,
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

/// Parenthesize compound expressions so template substitution keeps precedence.
fn paren(e: Expr) -> Expr {
    match e {
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
            let j = (i + 1..b.len()).find(|&j| !b[j].is_ascii_digit()).unwrap_or(b.len());
            text.push_str(ARG);
            text.push_str(&sql[i + 1..j]);
            i = j;
        } else {
            let c = sql[i..].chars().next().unwrap();
            text.push(c);
            i += c.len_utf8();
        }
    }
    let parsed = sqlglot_rust::parse_expr(&text).ok_or_else(|| format!("cannot parse SQL template `{sql}`"))?;
    let args: Vec<Expr> = args.into_iter().map(paren).collect();
    Ok(subst(parsed, &args))
}

/// Replace placeholder columns with arguments. `Expr::transform` does not
/// descend into a function's OVER clause, so window specs are handled here.
fn subst(e: Expr, args: &[Expr]) -> Expr {
    e.transform(&|e| subst_node(e, args))
}

fn subst_node(e: Expr, args: &[Expr]) -> Expr {
    match e {
        Expr::Column { table: None, ref name, .. } if name.to_ascii_lowercase().starts_with(ARG) => {
            match name[ARG.len()..].parse::<usize>().ok().and_then(|n| n.checked_sub(1)).and_then(|n| args.get(n)) {
                Some(a) => a.clone(),
                None => e,
            }
        }
        Expr::Function { name, args: fargs, distinct, filter, over: Some(spec), order_by, within_group } => {
            let spec = subst_spec(spec, args);
            Expr::Function { name, args: fargs, distinct, filter, over: Some(spec), order_by, within_group }
        }
        // Ranking functions such as ROW_NUMBER() parse as typed functions.
        Expr::TypedFunction { func, filter, over: Some(spec) } => {
            Expr::TypedFunction { func, filter, over: Some(subst_spec(spec, args)) }
        }
        other => other,
    }
}

fn subst_spec(mut spec: sqlglot_rust::ast::WindowSpec, args: &[Expr]) -> sqlglot_rust::ast::WindowSpec {
    spec.partition_by = spec.partition_by.into_iter().map(|p| subst(p, args)).collect();
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
    let all = |xs: &[ir::Expr]| xs.iter().map(|x| lower_expr(x, r)).collect::<Result<Vec<_>, _>>();
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
                let ps: Vec<String> =
                    spec.partition.iter().map(|p| Ok(place(lower_expr(p, r)?, &mut args))).collect::<Result<_, String>>()?;
                over.push(format!("PARTITION BY {}", ps.join(", ")));
            }
            if !spec.order.is_empty() {
                let os: Vec<String> = spec
                    .order
                    .iter()
                    .map(|(k, asc)| Ok(format!("{} {}", place(lower_expr(k, r)?, &mut args), if *asc { "ASC" } else { "DESC" })))
                    .collect::<Result<_, String>>()?;
                over.push(format!("ORDER BY {}", os.join(", ")));
            }
            if let Some(f) = spec.frame {
                over.push(format!("ROWS BETWEEN {} AND {}", bound(f.start), bound(f.end)));
            }
            template(&format!("{sql} OVER ({})", over.join(" ")), args)
        }
    }
}
