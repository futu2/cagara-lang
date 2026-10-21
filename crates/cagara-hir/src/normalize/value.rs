//! The reduction rules: normalize a typed-core expression to a [`Value`], and
//! apply a value to an argument.
//!
//! [`Normalizer::value`] matches on the *expression*; [`Normalizer::apply`]
//! matches on the *value*. Nothing here matches on the shape of an enclosing
//! expression to decide what an operation means: an application is an
//! application, and what it builds is decided by the operation at the head of
//! it once its arguments have arrived.

use cagara_syntax::ast::{Expr, ExprKind, Lit};

use super::arena::{Kept, Projection, QueryNodeKind, Rewrite};
use super::scalar;
use super::{
    frame, BodyRef, Closure, CombineFn, Control, Env, Label, Normalizer, Site, StageFn, StageSpec,
    Value,
};
use crate::check::NodeRef;
use crate::check::Resolved;
use crate::checked::{CheckedExpr, CheckedExprNode};
use crate::core::{Origin, ScalarType};

impl<'a> Normalizer<'a, 'a> {
    /// The value of one expression, under one environment.
    ///
    /// Every node pushes its frame, so the stack is exactly the path from the
    /// definition being normalized to the expression being reduced — which is
    /// what [`stage_site`](Self::stage_site) reads to find the application the
    /// checker recorded.
    pub(crate) fn value(
        &mut self,
        module: usize,
        expr: &'a Expr,
        env: &Env<'a>,
    ) -> Result<Value<'a>, Control> {
        let origin = Origin::new(module, expr.span);
        self.budget.spend(origin)?;
        let duplicate_frame = self.sites.last().is_some_and(|site| {
            site.module == module
                && site.def == self.defining
                && site.id == expr.id
                && site.span == expr.span
        });
        if !duplicate_frame {
            self.sites.push(Site {
                module,
                def: self.defining,
                id: expr.id,
                span: expr.span,
                named: true,
                parent: self.sites.last().map(|site| site.id),
            });
        }
        let result = self.value_at(module, expr, env);
        if !duplicate_frame {
            self.sites.pop();
        }
        result
    }

    fn value_at(
        &mut self,
        module: usize,
        expr: &'a Expr,
        env: &Env<'a>,
    ) -> Result<Value<'a>, Control> {
        let origin = Origin::new(module, expr.span);
        match &expr.kind {
            ExprKind::Field(side, column) => self.field(module, expr, *side, column),
            // A literal is a value the operations decide the meaning of. A
            // string literal in a `symbol` position names an affix or a column;
            // an int in a count position is a count; in an expression position
            // either is a constant. All three are the same value — a [`Label`] —
            // and the operation that consumes it says which: [`scalar_of`] is
            // what turns one back into an expression where one is wanted.
            ExprKind::Lit(Lit::Str(text)) => Ok(Value::Label(Label::text(text.clone()))),
            ExprKind::Lit(Lit::Int(value)) if Self::is_symbol_position(self, module, expr) => {
                Ok(Value::Label(Label::int(*value)))
            }
            ExprKind::Lit(Lit::Int(value)) => Ok(Value::Scalar(CheckedExpr::lit(
                crate::ir::Lit::Int(*value),
                origin,
            ))),
            ExprKind::Lit(lit) => Ok(Value::Scalar(CheckedExpr::lit(lit_of(lit), origin))),
            ExprKind::Name(_) | ExprKind::Proj(_, _) => self.name(module, expr, env),
            // A lambda is a function value, never applied: the application it
            // is part of supplies its arguments, and that application is the
            // frame below this one.
            ExprKind::Lambda(..) => self.lambda(module, expr, env),
            ExprKind::App(head, args) => {
                let mut value = self.value(module, head, env)?;
                for arg in args.iter() {
                    // An argument is part of this application, not an
                    // application of its own: its frame is not named, so a stage
                    // the checker recorded for a lambda body inside it is not
                    // mistaken for this one's.
                    let arg = self.argument(module, arg, env)?;
                    value = self.apply(value, arg)?;
                }
                Ok(value)
            }
            ExprKind::Case(_) => scalar::case(self, module, expr, env),
            ExprKind::Record(fields) => {
                // A record reaches normalization as a projection's fields, a
                // window specification, or an argument to a label-taking
                // operation. Its values are normalized here and its names travel
                // with them.
                let mut items = Vec::with_capacity(fields.len());
                let mut names = Vec::with_capacity(fields.len());
                for (name, field) in fields {
                    items.push(self.argument(module, field, env)?);
                    names.push(name.clone());
                }
                Ok(Value::Tuple {
                    items,
                    names: Some(names),
                })
            }
            ExprKind::List(items) => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(self.argument(module, item, env)?);
                }
                Ok(Value::Tuple {
                    items: values,
                    names: None,
                })
            }
            // A template reached through a name is a *definition*: its own body
            // is normalized by `body_value`, which knows its declared arity and
            // kind. Reaching one here would be a `sql` written inline, which
            // the language does not have.
            ExprKind::Sql(sql) => scalar::template(self, module, expr, sql),
            ExprKind::Primitive(name) => scalar::primitive(self, module, expr, name),
            ExprKind::Error => Err(Control::unsupported(
                origin,
                "the checker rejected an expression the compiler accepted",
            )),
        }
    }

    /// One expression normalized as another's argument.
    ///
    /// The same reduction as [`value`](Self::value), with the one difference
    /// that matters to identity: the frame is not named, because the
    /// application it belongs to is the site a node is recorded at.
    fn argument(
        &mut self,
        module: usize,
        expr: &'a Expr,
        env: &Env<'a>,
    ) -> Result<Value<'a>, Control> {
        let origin = Origin::new(module, expr.span);
        self.budget.spend(origin)?;
        self.sites.push(Site {
            module,
            def: self.defining,
            id: expr.id,
            span: expr.span,
            named: false,
            parent: self.sites.last().map(|site| site.id),
        });
        let result = self.value_at(module, expr, env);
        self.sites.pop();
        result
    }

    /// A column reference. The type is the checker's for this occurrence; the
    /// row it belongs to is whatever the enclosing stage checked it against,
    /// and the arena verifies the column exists when that stage is built.
    fn field(
        &mut self,
        module: usize,
        expr: &'a Expr,
        side: cagara_syntax::ast::Side,
        column: &str,
    ) -> Result<Value<'a>, Control> {
        let origin = Origin::new(module, expr.span);
        let Some(ty) = self.ty(module, expr) else {
            return Err(Control::unsupported(
                origin,
                "a column reference without a checked type",
            ));
        };
        Ok(Value::Scalar(CheckedExpr::column(
            side,
            column.to_string(),
            ty,
            origin,
        )))
    }

    /// A lambda: a closure over the current environment.
    fn lambda(
        &mut self,
        module: usize,
        expr: &'a Expr,
        env: &Env<'a>,
    ) -> Result<Value<'a>, Control> {
        let mut params = Vec::new();
        let mut body = expr;
        while let ExprKind::Lambda(parameter, next) = &body.kind {
            params.push(parameter.clone());
            body = next;
        }
        Ok(Value::Closure(Closure {
            params: std::sync::Arc::new(params),
            bound: 0,
            body: BodyRef {
                module,
                def: self.defining,
                expr: body,
            },
            env: std::sync::Arc::new(env.clone()),
            // Keep the application that is supplying this closure. The body
            // may construct a relational node after the lambda is reduced
            // away, and that node still belongs to this use site in the
            // checker's stage index.
            site: self.definition_site.or_else(|| self.call_site().copied()),
        }))
    }

    /// A name: a local binding, or the definition the checker resolved it to.
    ///
    /// This is where an alias disappears: an alias *is* its target, so
    /// following it is this call rather than a separate walk.
    fn name(&mut self, module: usize, expr: &'a Expr, env: &Env<'a>) -> Result<Value<'a>, Control> {
        let origin = Origin::new(module, expr.span);
        let label = match &expr.kind {
            ExprKind::Name(label) | ExprKind::Proj(_, label) => label,
            _ => {
                return Err(Control::unsupported(
                    origin,
                    "a projection has no local name",
                ))
            }
        };
        if let Some(value) = env.lookup(label) {
            if let Value::Scalar(expr) = value {
                for _ in 0..expr.node_count() {
                    self.budget.spend(origin)?;
                }
            }
            return Ok(value.clone());
        }
        match self.resolution(module, expr) {
            Some(Resolved::Def((target_module, target_def))) => {
                self.definition(target_module, target_def, origin)
            }
            Some(Resolved::Overloads {
                module: target_module,
                defs,
            }) => self.overloads(target_module, module, expr, &defs),
            // The checker records a primitive only for a `primitive "…"` body,
            // which is read as a body form rather than as a name.
            Some(Resolved::Primitive) => Err(Control::unsupported(
                origin,
                "a primitive used as an expression",
            )),
            // A body no use reaches — a primitive definition's own — has no
            // recorded resolution, so the name is looked up in the scope of the
            // definition being normalized, which is the scope it was written in.
            // The first definition of that name is enough: narrowing the
            // overloads is a *use* site's job, and there is none here.
            None => match self.scope_definition(module, label) {
                Some((target_module, target_def)) => {
                    self.definition(target_module, target_def, origin)
                }
                None => Ok(Value::Name(label.clone())),
            },
        }
    }

    /// The candidates of an overloaded name: the one the checker selected at
    /// this use, or — when the use site itself is still open — each candidate,
    /// filtered by whichever one applies.
    fn overloads(
        &mut self,
        target_module: usize,
        use_module: usize,
        expr: &'a Expr,
        defs: &[usize],
    ) -> Result<Value<'a>, Control> {
        let origin = Origin::new(use_module, expr.span);
        if let Some(node) = self.typed(use_module, expr) {
            for (_, choice) in &node.choices {
                if let crate::check::Choice::Def(choice_module, target_def) = choice {
                    if target_module == *choice_module && defs.contains(target_def) {
                        return self.definition(*choice_module, *target_def, origin);
                    }
                }
            }
        }
        let mut values = Vec::with_capacity(defs.len());
        for def in defs {
            match self.definition(target_module, *def, origin) {
                Ok(value) => values.push(value),
                Err(Control::Unsupported { .. }) => continue,
                Err(error) => return Err(error),
            }
        }
        match values.len() {
            0 => Err(Control::unsupported(
                origin,
                "no overload candidate applies",
            )),
            1 => Ok(values.remove(0)),
            _ => Ok(Value::Overloads(values)),
        }
    }

    /// Normalize a definition's body.
    ///
    /// The budget charges the path; a definition already on it is a cycle, and
    /// is refused before the budget could only say "too deep".
    ///
    /// The *site* is restored afterwards: a definition normalizes to a value —
    /// a primitive's stage, a lambda, an alias's target — and applying that
    /// value is something the calling expression does. Leaving the callee's sites
    /// current would stamp the call in the caller with the callee's
    pub(crate) fn definition(
        &mut self,
        module: usize,
        def: usize,
        use_origin: Origin,
    ) -> Result<Value<'a>, Control> {
        let key = (module, def);
        if self.active.contains(&key) {
            let name = &self.input.module(module).def(def).name;
            return Err(Control::Program(
                crate::core::Error::new(format!(
                    "`{name}` refers to itself; recursion is not supported"
                ))
                .at(use_origin),
            ));
        }
        self.budget.enter(use_origin)?;
        self.active.push(key);
        let previous = std::mem::replace(&mut self.defining, def);
        let previous_site = self.definition_site;
        self.definition_site = self.application_site(use_origin);
        let body = &self.input.module(module).def(def).body;
        let result = self.body_value(module, def, body, &Env::default());
        self.definition_site = previous_site;
        self.defining = previous;
        self.active.pop();
        self.budget.leave();
        result
    }

    /// The value of a definition's declared body, by its form.
    ///
    /// A definition body carries its own identity: the body of a query
    /// definition is the query, and when that definition is a table or a stage
    /// application the node built here is blamed at this node.
    pub(crate) fn body_value(
        &mut self,
        module: usize,
        def: usize,
        body: &'a Expr,
        env: &Env<'a>,
    ) -> Result<Value<'a>, Control> {
        self.budget.spend(Origin::new(module, body.span))?;
        self.sites.push(Site {
            module,
            def,
            id: body.id,
            span: body.span,
            named: true,
            parent: None,
        });
        let result = match &body.kind {
            ExprKind::Primitive(name) => scalar::primitive(self, module, body, name),
            ExprKind::Sql(sql) => scalar::template_value(self, module, body, sql, def),
            // Any other body is an ordinary expression: an alias follows, a
            // lambda is a function value, an application is reduced.
            _ => self.value(module, body, env),
        };
        self.sites.pop();
        result
    }
    /// Apply one argument to a value: the single reduction step.
    pub(crate) fn apply(&mut self, value: Value<'a>, arg: Value<'a>) -> Result<Value<'a>, Control> {
        let origin = self.origin();
        self.budget.spend(origin)?;
        match value {
            Value::Closure(closure) => self.apply_closure(closure, arg),
            Value::Stage(stage) => self.apply_stage(stage, arg),
            Value::Combine(combine) => self.apply_combine(combine, arg),
            Value::Template(template) => scalar::apply_template(self, template, arg),
            Value::Table { args, origin, .. } => self.apply_table(&args, arg, origin),
            Value::Direction {
                ascending,
                mut args,
                origin: _,
            } => {
                let key = self.as_expr(arg, self.call_module(), self.call_expr()?)?;
                args.clear();
                Ok(Value::SortKey(key, ascending))
            }
            Value::Group { arg: None, origin } => {
                let key = self.as_expr(arg, self.call_module(), self.call_expr()?)?;
                CheckedExpr::group(key, origin)
                    .map(Value::Scalar)
                    .map_err(Control::Program)
            }
            Value::Membership {
                list: None,
                value,
                origin,
            } => {
                let (module, list) = self.list_origin()?;
                let Value::Tuple { items, .. } = &arg else {
                    return Err(Control::unsupported(origin, "`in` takes a list"));
                };
                let mut lowered = Vec::with_capacity(items.len());
                for item in items.iter() {
                    let item = self.as_expr(item.clone(), module, list)?;
                    lowered.push(item);
                }
                Ok(Value::Membership {
                    list: Some(lowered),
                    value,
                    origin,
                })
            }
            Value::Membership {
                list: Some(list),
                origin,
                ..
            } => {
                let value = self.as_expr(arg, self.call_module(), self.call_expr()?)?;
                CheckedExpr::in_(value, list, false, origin)
                    .map(Value::Scalar)
                    .map_err(Control::Program)
            }
            Value::Fold {
                partial,
                origin,
                spec,
            } => frame::fold(self, &partial, spec, arg, origin),
            // A name that reached application unbound: the lambda that applied
            // it did not receive its argument. On an accepted program the
            // closure carries it in the environment, so this is a capability
            // gap, not a wrong program.
            Value::Name(label) => Err(Control::unsupported(
                origin,
                format!("`{label}` was applied before it was given a value"),
            )),
            Value::Overloads(candidates) => {
                for candidate in candidates {
                    if let Ok(applied) = self.apply(candidate, arg.clone()) {
                        return Ok(applied);
                    }
                }
                Err(Control::unsupported(
                    origin,
                    "no overload candidate accepts this argument",
                ))
            }
            other => Err(Control::unsupported(
                origin,
                format!("this is not a function: it is {}", value_class(&other)),
            )),
        }
    }

    /// β-reduction.
    ///
    /// A curried lambda is one closure with every parameter listed, so binding
    /// the next parameter is the reduction step, and the body is normalized when
    /// the last one arrives — exactly as nested lambdas would reduce, one step
    /// at a time.
    fn apply_closure(
        &mut self,
        closure: Closure<'a>,
        arg: Value<'a>,
    ) -> Result<Value<'a>, Control> {
        let origin = self.origin();
        let Some(parameter) = closure.params.get(closure.bound) else {
            return Err(Control::unsupported(
                origin,
                "a function was applied to more arguments than it takes",
            ));
        };
        let env = closure.env.bind(parameter, arg);
        let bound = closure.bound + 1;
        if bound < closure.params.len() {
            return Ok(Value::Closure(Closure {
                params: closure.params,
                bound,
                body: closure.body,
                env: std::sync::Arc::new(env),
                site: closure.site,
            }));
        }
        let module = closure.body.module;
        let previous = std::mem::replace(&mut self.defining, closure.body.def);
        let key = (module, closure.body.def);
        let entered = if !self.active.contains(&key)
            && definition_reaches(
                self.input,
                module,
                closure.body.def,
                module,
                closure.body.def,
                &mut Vec::new(),
            ) {
            self.active.push(key);
            true
        } else {
            false
        };
        // The body is normalized as the definition that wrote the lambda: the
        // frames it pushes are that definition's nodes.
        let saved = self.sites.clone();
        if let Some(site) = closure.site {
            self.sites.push(site);
        }
        let result = self.body_value(module, closure.body.def, closure.body.expr, &env);
        self.sites = saved;
        if entered {
            self.active.pop();
        }
        self.defining = previous;
        result
    }

    /// `table "s" "t"`: two labels and the row the checker closed for the call.
    fn apply_table(
        &mut self,
        partial: &[Value<'static>],
        arg: Value<'a>,
        origin: Origin,
    ) -> Result<Value<'a>, Control> {
        // The row is the checker's answer for the table call, so it is looked up
        // at the expression this application belongs to — not at the primitive's
        // origin, which is the prelude.
        // The row the checker closed for the table call, found by the site of the
        // application that consumed the primitive value.
        let call_row = self.call_site().and_then(|site| {
            self.input
                .type_check()
                .typed_body(site.module, site.def)
                .and_then(|tree| tree.node_at(site.span))
                .and_then(|node| node.row())
        });
        let Value::Label(label) = arg else {
            return Err(Control::unsupported(
                origin,
                "`table` takes the schema and table name as strings",
            ));
        };
        let mut args = partial.to_vec();
        args.push(Value::Label(label));
        if args.len() < 2 {
            return Ok(Value::Table {
                args,
                row: call_row.clone(),
                origin,
            });
        }
        let (Some(Value::Label(schema)), Some(Value::Label(name))) = (args.first(), args.get(1))
        else {
            return Err(Control::unsupported(
                origin,
                "`table` takes the schema and table name as strings",
            ));
        };
        let (schema, name) = (schema.text.clone(), name.text.clone());
        // The columns are the checker's row for the *use site's* table call, and
        // the use site is the call this reduction is part of — the primitive
        // itself lives in the prelude and has no row of its own.
        let row = call_row;
        let call_origin = self.origin();
        let site = NodeRef::new(call_origin.module, self.defining, self.call_id());
        self.builder
            .table(site, call_origin, schema, name, row)
            .map(Value::Query)
            .map_err(Control::Program)
    }

    /// Apply an argument to a stage waiting for its input. When the last
    /// argument arrives the stage constructs its relational node.
    fn apply_stage(&mut self, mut stage: StageFn, arg: Value<'a>) -> Result<Value<'a>, Control> {
        stage.args.push(own(&arg));
        if stage.args.len() < stage.spec.arity {
            return Ok(Value::Stage(stage));
        }
        let args = std::mem::take(&mut stage.args);
        self.build_stage(stage.spec, stage.site, args)
    }

    /// Apply an argument to a partially applied join or set operation.
    fn apply_combine(
        &mut self,
        mut combine: CombineFn,
        arg: Value<'a>,
    ) -> Result<Value<'a>, Control> {
        let collected = own(&arg);
        let args = match &mut combine {
            CombineFn::Join { args, .. } | CombineFn::Set { args, .. } => {
                args.push(collected);
                args.clone()
            }
        };
        if args.len() < combine.arity() {
            return Ok(Value::Combine(combine));
        }
        self.build_combine(combine, args)
    }

    // ── construction ────────────────────────────────────────────────────────

    /// Construct the relational node a saturated stage denotes.
    ///
    /// The arguments arrive in declaration order and the *last* one is the
    /// piped input: every stage in the prelude takes its subject last, so the
    /// pipeline operator supplies the query as the final argument.
    fn build_stage(
        &mut self,
        spec: StageSpec,
        operation_site: Option<Site>,
        args: Vec<Value<'static>>,
    ) -> Result<Value<'a>, Control> {
        let (origin, site) = self.stage_site(spec.kind, operation_site);
        let (input, arguments) = args
            .split_last()
            .ok_or_else(|| Control::unsupported(origin, "a stage with no arguments"))?;
        let Some(input) = input.as_query() else {
            return Err(Control::unsupported(
                origin,
                format!(
                    "`{:?}` takes a query as its input, and this is {}",
                    spec.kind,
                    value_class(input)
                ),
            ));
        };
        let built = match spec.kind {
            QueryNodeKind::Prefix
            | QueryNodeKind::Suffix
            | QueryNodeKind::Omit
            | QueryNodeKind::MapValue => {
                let Some(Value::Label(label)) = arguments.first() else {
                    return Err(Control::unsupported(
                        origin,
                        format!("`{:?}` takes a label", spec.kind),
                    ));
                };
                let rewrite = match spec.kind {
                    QueryNodeKind::Prefix => Rewrite::Rename {
                        affix: label.text.clone(),
                        prefix: true,
                    },
                    QueryNodeKind::Suffix => Rewrite::Rename {
                        affix: label.text.clone(),
                        prefix: false,
                    },
                    QueryNodeKind::Omit => Rewrite::Omit(label.text.clone()),
                    _ => Rewrite::MapValue(label.text.clone()),
                };
                self.builder.rewrite(site, origin, input, rewrite)
            }
            QueryNodeKind::Where => {
                let Some(predicate) = arguments.first() else {
                    return self.wrong_argument(origin, "a predicate");
                };
                let predicate = self.arg_expr(predicate, origin)?;
                self.builder
                    .keep_row(site, origin, input, Kept::Filter(predicate))
            }
            QueryNodeKind::Order => {
                let Some(Value::Tuple { items, .. }) = arguments.first() else {
                    return self.wrong_argument(origin, "a list of sort keys");
                };
                let mut keys = Vec::with_capacity(items.len());
                for item in items {
                    match item {
                        Value::SortKey(expr, ascending) => keys.push((expr.clone(), *ascending)),
                        Value::Scalar(expr) => keys.push((expr.clone(), true)),
                        other => {
                            return Err(Control::unsupported(
                                origin,
                                format!("`order` cannot sort by {}", value_class(other)),
                            ))
                        }
                    }
                }
                self.builder
                    .keep_row(site, origin, input, Kept::Order(keys))
            }
            QueryNodeKind::Limit | QueryNodeKind::Offset => {
                let Some(count) = arguments.first() else {
                    return self.wrong_argument(origin, "a count");
                };
                let count = self.as_count(
                    count,
                    origin,
                    if spec.kind == QueryNodeKind::Limit {
                        "limit"
                    } else {
                        "offset"
                    },
                )?;
                let kept = if spec.kind == QueryNodeKind::Limit {
                    Kept::Limit(count)
                } else {
                    Kept::Offset(count)
                };
                self.builder.keep_row(site, origin, input, kept)
            }
            QueryNodeKind::Distinct => self.builder.keep_row(site, origin, input, Kept::Distinct),
            QueryNodeKind::Select | QueryNodeKind::Update | QueryNodeKind::Agg => {
                let Some(fields) = arguments.first() else {
                    return self.wrong_argument(origin, "a record of fields");
                };
                let fields = self.projection_fields(origin, fields)?;
                let projection = match spec.kind {
                    QueryNodeKind::Select => Projection::Select,
                    QueryNodeKind::Update => Projection::Update,
                    _ => Projection::Agg,
                };
                self.builder
                    .project(site, origin, input, projection, fields)
            }
            QueryNodeKind::Table => {
                return Err(Control::unsupported(
                    origin,
                    "a table is not a pipeline stage",
                ))
            }
            QueryNodeKind::Join(_) | QueryNodeKind::Set(_) => {
                return Err(Control::unsupported(
                    origin,
                    "a join or set operation is not a pipeline stage",
                ))
            }
        };
        built.map(Value::Query).map_err(Control::Program)
    }

    /// An accumulated argument as an expression: a literal is still a
    /// literal, so it is built here, and the phase a stage needs is the one
    /// the checker already gave the expression the operation consumes — a
    /// literal is `const` until it is measured against that expression's
    /// own phase. See [`rephase`].
    fn arg_expr(&self, value: &Value<'a>, origin: Origin) -> Result<CheckedExpr, Control> {
        match value {
            Value::Scalar(expr) => Ok(expr.clone()),
            Value::Label(label) => {
                let expr = scalar_of(&Value::Label(label.clone()), ScalarType::Unknown, origin)
                    .ok_or_else(|| {
                        Control::unsupported(origin, "a label where an expression is required")
                    })?;
                Ok(match self.call_site_phase() {
                    Some(phase) => rephase(expr, phase),
                    None => expr,
                })
            }
            other => Err(Control::unsupported(
                origin,
                format!(
                    "this argument is not an expression: it is {}",
                    value_class(other)
                ),
            )),
        }
    }

    fn wrong_argument<T>(&self, origin: Origin, what: &str) -> Result<T, Control> {
        Err(Control::unsupported(
            origin,
            format!("this stage takes {what}"),
        ))
    }

    /// A projection's fields: the record's values, named by the record the
    /// checker typed. Both come from the same argument, so an output row cannot
    /// be paired with another projection's fields.
    fn projection_fields(
        &mut self,
        origin: Origin,
        record: &Value<'a>,
    ) -> Result<Vec<(String, CheckedExpr)>, Control> {
        let Value::Tuple { items, names } = record else {
            return self.wrong_argument(origin, "a record of fields");
        };
        let Some(names) = names.clone() else {
            return self.wrong_argument(origin, "a record of fields");
        };
        let mut expressions = Vec::with_capacity(items.len());
        for value in items.iter() {
            expressions.push(self.arg_expr(value, origin)?);
        }
        if names.len() != expressions.len() {
            return Err(Control::unsupported(
                origin,
                "a projection has fewer names than fields",
            ));
        }
        Ok(names.into_iter().zip(expressions).collect())
    }

    /// Construct the node a saturated join or set operation denotes.
    fn build_combine(
        &mut self,
        combine: CombineFn,
        args: Vec<Value<'static>>,
    ) -> Result<Value<'a>, Control> {
        let operation_site = match &combine {
            CombineFn::Join { site, .. } | CombineFn::Set { site, .. } => *site,
        };
        let (origin, site) = self.stage_site(
            match &combine {
                CombineFn::Join { kind, .. } => QueryNodeKind::Join(*kind),
                CombineFn::Set { kind, .. } => QueryNodeKind::Set(*kind),
            },
            operation_site,
        );
        match combine {
            CombineFn::Join { kind, .. } => {
                // `innerJoin` is `query s -> expr (join r s) bool -> query r`,
                // so the joined input arrives first, the predicate second, and
                // the piped input last.
                let (Some(Value::Query(joined)), Some(Value::Scalar(on))) =
                    (args.first(), args.get(1))
                else {
                    return Err(Control::unsupported(
                        origin,
                        "a join takes a query and a predicate",
                    ));
                };
                let Some(Value::Query(input)) = args.get(2) else {
                    return Err(Control::unsupported(origin, "a join needs its piped input"));
                };
                let (joined, input, on) = (*joined, *input, on.clone());
                self.builder
                    .join(site, origin, kind, input, joined, on)
                    .map(Value::Query)
                    .map_err(Control::Program)
            }
            CombineFn::Set { kind, .. } => {
                let (Some(Value::Query(left)), Some(Value::Query(right))) =
                    (args.first(), args.get(1))
                else {
                    return Err(Control::unsupported(
                        origin,
                        "a set operation takes two queries",
                    ));
                };
                let (left, right) = (*left, *right);
                self.builder
                    .set(site, origin, kind, left, right)
                    .map(Value::Query)
                    .map_err(Control::Program)
            }
        }
    }

    /// Where an operation's node is written, and its identity.
    ///
    /// A stage is applied by a *definition* — the prelude, when the stage is a
    /// primitive, which is why this cannot simply read the current site: a
    /// primitive's own body is the prelude's. The checker records one
    /// `RowStageView` per application, stamped with the definition the
    /// application was written in and the application's `ExprId`, and that
    /// record is what this returns — in particular its `span`, because a stage
    /// reached through a helper must be blamed where the helper wrote it, not
    /// where the helper was called.
    ///
    /// The lookup is by kind and expression: the checker recorded the
    /// application under the definition whose body spells it, stamped with the
    /// application's `ExprId`. The innermost *named* frame is that application —
    /// a lambda or an argument does not name one — so the record is found by
    /// walking the stack outwards and taking the first stage of this kind
    /// recorded at a named frame.
    ///
    /// When there is no record — a stage the checker did not record, which on
    /// an accepted program does not happen — the innermost named frame is the
    /// best available answer for both the origin and the identity.
    fn stage_site(&self, kind: QueryNodeKind, operation_site: Option<Site>) -> (Origin, NodeRef) {
        if let Some(site) = operation_site {
            // The primitive name and its application share a source span in
            // the syntax walk but receive distinct expression ids. The
            // checker stamps the application (the parent of the name frame),
            // so use that id for the stable node identity.
            let operation_id = site.parent.unwrap_or(site.id);
            let node = NodeRef::new(site.module, site.def, operation_id);
            if let Some(stage) = self
                .input
                .type_check()
                .row_stages(site.module, site.def)
                .find(|stage| stage.node == node && Self::kind_of_stage(stage.kind) == kind)
            {
                let span = self
                    .input
                    .type_check()
                    .typed_node(site.module, operation_id)
                    .map_or(stage.span, |typed| typed.span);
                return (Origin::new(stage.node.definition_module, span), stage.node);
            }
            if matches!(kind, QueryNodeKind::Join(_) | QueryNodeKind::Set(_)) {
                if let Some(combine) = self
                    .input
                    .type_check()
                    .typed_node(site.module, operation_id)
                    .and_then(|node| node.combine.as_ref())
                {
                    let span = self
                        .input
                        .type_check()
                        .typed_node(site.module, operation_id)
                        .map_or(site.span, |typed| typed.span);
                    return (Origin::new(site.module, span), combine.node);
                }
            }
        }
        for site in self.sites.iter().rev().filter(|site| site.named) {
            let found = self
                .input
                .type_check()
                .row_stages(site.module, site.def)
                .find(|stage| stage.site == site.id && Self::kind_of_stage(stage.kind) == kind);
            if let Some(stage) = found {
                return (
                    Origin::new(stage.node.definition_module, stage.span),
                    stage.node,
                );
            }
        }
        match self.call_site() {
            Some(site) => (site.origin(), NodeRef::new(site.module, site.def, site.id)),
            None => {
                let origin = self.origin();
                (origin, NodeRef::new(origin.module, self.defining, 0))
            }
        }
    }

    /// The enclosing application at a definition use. The innermost frame is
    /// normally the name being resolved; the application frame is its parent
    /// and is the identity recorded by the checker for stages and combines.
    fn application_site(&self, use_origin: Origin) -> Option<Site> {
        self.sites
            .iter()
            .rev()
            .find(|site| site.origin() == use_origin)
            .or_else(|| {
                self.sites.iter().rev().find(|site| {
                    site.span.start <= use_origin.span.start && site.span.end >= use_origin.span.end
                })
            })
            .copied()
    }

    /// The stage kind a recorded stage denotes.
    ///
    /// The checker's vocabulary (`RowStage`) and the arena's (`QueryNodeKind`)
    /// name the same operations; `prefix`/`suffix` are one operation on each
    /// side, spelled separately because their payloads differ.
    fn kind_of_stage(stage: crate::check::RowStage) -> QueryNodeKind {
        use crate::check::RowStage;
        match stage {
            RowStage::Where => QueryNodeKind::Where,
            RowStage::Select => QueryNodeKind::Select,
            RowStage::Update => QueryNodeKind::Update,
            RowStage::Omit => QueryNodeKind::Omit,
            RowStage::Prefix => QueryNodeKind::Prefix,
            RowStage::Suffix => QueryNodeKind::Suffix,
            RowStage::MapValue => QueryNodeKind::MapValue,
            RowStage::Agg => QueryNodeKind::Agg,
            RowStage::Order => QueryNodeKind::Order,
            RowStage::Limit => QueryNodeKind::Limit,
            RowStage::Offset => QueryNodeKind::Offset,
            RowStage::Distinct => QueryNodeKind::Distinct,
        }
    }

    // ── facts ───────────────────────────────────────────────────────────────

    /// Whether the checker typed this occurrence as a label.
    ///
    /// A `symbol` argument — an affix, a column to drop, a value wrapper — and
    /// an int in a count position are the same thing to the checker: a type
    /// that is not a scalar at all. Asking the checker rather than the template
    /// is what keeps `limit 2` and `select { n = 2 }` apart without either
    /// operation being recognised by name.
    fn is_symbol_position(&self, module: usize, expr: &Expr) -> bool {
        self.typed(module, expr)
            .is_some_and(|node| node.ty().is_none())
    }

    /// The phase of the expression the application being reduced was written
    /// at.
    ///
    /// An argument's phase is the checker's, but the *place* the operation
    /// sits in decides what a constant is allowed to be: the same literal is
    /// row phase in `where` and aggregate phase in `agg`, and only the
    /// application knows which. That is the fact [`rephase`] reads.
    pub(crate) fn call_site_phase(&self) -> Option<crate::ir::Phase> {
        let site = self.sites.last()?;
        self.input
            .type_check()
            .typed_body(site.module, site.def)
            .and_then(|tree| tree.node_at(site.span))
            .and_then(|node| node.scalar_phase())
    }

    /// An argument as a checked expression.
    ///
    /// Most arguments already are one. A literal is a [`Label`], because a
    /// literal's *meaning* is the operation's to decide, so it becomes an
    /// expression again here with the type the checker gave this occurrence.
    pub(crate) fn as_expr(
        &self,
        value: Value<'a>,
        module: usize,
        expr: &Expr,
    ) -> Result<CheckedExpr, Control> {
        let origin = Origin::new(module, expr.span);
        match value {
            Value::Scalar(checked) => Ok(checked),
            Value::Label(label) => {
                let ty = self.ty(module, expr).unwrap_or(ScalarType::Unknown);
                let built = scalar_of(&Value::Label(label), ty, origin).ok_or_else(|| {
                    Control::unsupported(origin, "a label where an expression is required")
                })?;
                Ok(match self.call_site_phase() {
                    Some(phase) => rephase(built, phase),
                    None => built,
                })
            }
            other => Err(Control::unsupported(
                origin,
                format!(
                    "this argument is not an expression: it is {}",
                    value_class(&other)
                ),
            )),
        }
    }

    /// A value as a count: an int literal, or a `symbol` parameter bound to
    /// one.
    pub(crate) fn as_count(
        &self,
        value: &Value<'a>,
        origin: Origin,
        operation: &str,
    ) -> Result<i64, Control> {
        let invalid = |at| {
            Control::Program(crate::core::Error::new(format!("{operation} expects an int")).at(at))
        };
        match value {
            Value::Label(label) => {
                if let Some(int) = label.int {
                    return Ok(int);
                }
                label.text.parse().map_err(|_| invalid(origin))
            }
            // A literal that occurs in a closed checker application can be
            // typed as an ordinary scalar rather than a symbol. Counts still
            // consume the literal value, so accept that representation too.
            Value::Scalar(expr) => {
                let (_, _, node, _) = expr.clone().into_parts();
                match node {
                    CheckedExprNode::Lit(crate::ir::Lit::Int(value)) => Ok(value),
                    _ => Err(invalid(expr.origin())),
                }
            }
            _ => Err(invalid(origin)),
        }
    }
    pub(crate) fn ty(&self, module: usize, expr: &Expr) -> Option<ScalarType> {
        self.ty_at(Origin::new(module, expr.span))
    }
}

/// Whether expanding a definition can reach itself through resolved names.
///
/// Only genuinely recursive lambda bodies need to stay active while reduced.
/// A non-recursive combinator such as the prelude's `_&_` may be applied again
/// by a function argument, and treating that ordinary higher-order expansion
/// as recursion rejects valid pipelines.
fn definition_reaches(
    input: crate::compile::CompilerInput<'_>,
    module: usize,
    def: usize,
    target_module: usize,
    target_def: usize,
    seen: &mut Vec<(usize, usize)>,
) -> bool {
    if seen.contains(&(module, def)) {
        return false;
    }
    seen.push((module, def));
    let body = &input.module(module).def(def).body;
    fn visit(
        input: crate::compile::CompilerInput<'_>,
        module: usize,
        expr: &Expr,
        target_module: usize,
        target_def: usize,
        seen: &mut Vec<(usize, usize)>,
    ) -> bool {
        if matches!(expr.kind, ExprKind::Name(_) | ExprKind::Proj(_, _)) {
            match input.type_check().resolution(module, expr.id) {
                Some(Resolved::Def((next_module, next_def))) => {
                    if (*next_module, *next_def) == (target_module, target_def)
                        || definition_reaches(
                            input,
                            *next_module,
                            *next_def,
                            target_module,
                            target_def,
                            seen,
                        )
                    {
                        return true;
                    }
                }
                Some(Resolved::Overloads {
                    module: next_module,
                    defs,
                }) if defs.iter().any(|next_def| {
                    (*next_module, *next_def) == (target_module, target_def)
                        || definition_reaches(
                            input,
                            *next_module,
                            *next_def,
                            target_module,
                            target_def,
                            seen,
                        )
                }) =>
                {
                    return true
                }
                _ => {}
            }
        }
        crate::check::child_exprs(expr)
            .into_iter()
            .any(|child| visit(input, module, child, target_module, target_def, seen))
    }
    visit(input, module, body, target_module, target_def, seen)
}
/// A value moved into an accumulator's argument list.
///
/// Every value an accumulator can hold is owned already — an index, an
/// expression, a label, a specification — so this only erases the lifetime. A
/// closure, which an accumulator cannot receive on an accepted program, becomes
/// an empty label rather than being lost silently.
pub(crate) fn own(value: &Value<'_>) -> Value<'static> {
    match value {
        Value::Query(id) => Value::Query(*id),
        Value::Scalar(expr) => Value::Scalar(expr.clone()),
        Value::Label(label) => Value::Label(label.clone()),
        Value::SortKey(expr, ascending) => Value::SortKey(expr.clone(), *ascending),
        Value::WinSpec(spec) => Value::WinSpec(spec.clone()),
        Value::Frame(frame) => Value::Frame(*frame),
        Value::Bound(bound) => Value::Bound(*bound),
        Value::Stage(stage) => Value::Stage(stage.clone()),
        Value::Combine(combine) => Value::Combine(combine.clone()),
        Value::Template(template) => Value::Template(template.clone()),
        Value::Table { args, row, origin } => Value::Table {
            args: args.iter().map(own).collect(),
            row: row.clone(),
            origin: *origin,
        },
        Value::Direction {
            ascending,
            args,
            origin,
        } => Value::Direction {
            ascending: *ascending,
            args: args.iter().map(own).collect(),
            origin: *origin,
        },
        Value::Group { arg, origin } => Value::Group {
            arg: arg.clone(),
            origin: *origin,
        },
        Value::Membership {
            list,
            value,
            origin,
        } => Value::Membership {
            list: list.clone(),
            value: value.clone(),
            origin: *origin,
        },
        Value::Fold {
            partial,
            origin,
            spec,
        } => Value::Fold {
            partial: partial.iter().map(own).collect(),
            origin: *origin,
            spec: *spec,
        },
        Value::Overloads(values) => Value::Overloads(values.iter().map(own).collect()),
        Value::Tuple { items, names } => Value::Tuple {
            items: items.iter().map(own).collect(),
            names: names.clone(),
        },
        Value::Closure(_) => Value::Label(Label::text("")),
        Value::Name(name) => Value::Name(name.clone()),
    }
}

/// A short name for what a value denotes, for a diagnostic.
pub(crate) fn value_class(value: &Value<'_>) -> &'static str {
    match value {
        Value::Closure(_) => "a function",
        Value::Name(_) => "a name",
        Value::Query(_) => "a query",
        Value::Scalar(_) => "an expression",
        Value::Stage(_) => "a stage",
        Value::Combine(_) => "a join or set operation",
        Value::Template(_) => "a SQL template",
        Value::Table { .. } => "a table",
        Value::Label(_) => "a label",
        Value::Direction { .. } => "a sort direction",
        Value::SortKey(..) => "a sort key",
        Value::Group { .. } => "a grouping key",
        Value::Membership { .. } => "a membership test",
        Value::WinSpec(_) => "a window specification",
        Value::Frame(_) => "a row frame",
        Value::Bound(_) => "a frame bound",
        Value::Fold { .. } => "a frame or offset",
        Value::Overloads(_) => "an overloaded name",
        Value::Tuple { .. } => "a record or list",
    }
}

/// The literal a source literal denotes.
fn lit_of(lit: &Lit) -> crate::ir::Lit {
    match lit {
        Lit::Int(value) => crate::ir::Lit::Int(*value),
        Lit::Float(value) => crate::ir::Lit::Float(value.clone()),
        Lit::Str(value) => crate::ir::Lit::Str(value.clone()),
        Lit::Bool(value) => crate::ir::Lit::Bool(*value),
    }
}

/// The stage a primitive is, when it is a pipeline stage.
///
/// The mapping from a primitive to a stage is the checker's own
/// (`RowStage::of_prim`), so an operation cannot be a stage for the checker and
/// something else here.
pub(crate) fn stage_spec(prim: crate::primitive::Prim) -> Option<StageSpec> {
    use crate::check::RowStage;
    let kind = match RowStage::of_prim(prim)? {
        RowStage::Where => QueryNodeKind::Where,
        RowStage::Select => QueryNodeKind::Select,
        RowStage::Update => QueryNodeKind::Update,
        RowStage::Omit => QueryNodeKind::Omit,
        RowStage::Prefix => QueryNodeKind::Prefix,
        RowStage::Suffix => QueryNodeKind::Suffix,
        RowStage::MapValue => QueryNodeKind::MapValue,
        RowStage::Agg => QueryNodeKind::Agg,
        RowStage::Order => QueryNodeKind::Order,
        RowStage::Limit => QueryNodeKind::Limit,
        RowStage::Offset => QueryNodeKind::Offset,
        RowStage::Distinct => QueryNodeKind::Distinct,
    };
    Some(StageSpec {
        kind,
        arity: prim.arity(),
    })
}

/// A value as a checked expression, when it denotes a constant.
///
/// A literal reduces to a label, because the operation that consumes it decides
/// what it names — an affix, a column to drop, a wrapper, a count, or a
/// constant. An operation that wants an *expression* asks for one here, and gets
/// the literal back with the type the checker gave the occurrence.
pub(crate) fn scalar_of(value: &Value<'_>, ty: ScalarType, origin: Origin) -> Option<CheckedExpr> {
    let Value::Label(label) = value else {
        return None;
    };
    let checked = match (label.int, &ty) {
        (Some(int), ScalarType::Float) => crate::ir::Lit::Float(format!("{int}.0")),
        (Some(int), _) => crate::ir::Lit::Int(int),
        (None, _) => crate::ir::Lit::Str(label.text.clone()),
    };
    Some(CheckedExpr::lit(checked, origin))
}

/// A constant lifted into the phase of the expression it is part of.
///
/// A literal is `const` on its own, and every rule that combines phases
/// ([`crate::rules::mix`]) treats `const` as "whatever the other side is". The
/// checked layer has no rule for lifting a constant on its own — a literal is
/// written by the expression it appears in, so the constructor of that
/// expression is what decides its phase. The normalizer builds the literal
/// first and the expression that consumes it second, so this is where the
/// decision the constructor would have made is recorded.
///
/// It is the *call site's* phase, not the enclosing stage's: `ifThenElse p 1 2`
/// inside `agg` is aggregate phase because the checker typed that application
/// aggregate, and a constant there is aggregate for the same reason.
pub(crate) fn rephase(expr: CheckedExpr, phase: crate::ir::Phase) -> CheckedExpr {
    if expr.phase() == phase || expr.phase() != crate::ir::Phase::Const {
        return expr;
    }
    let original = expr.clone();
    // The node is rebuilt from the erased form — the only construction outside
    // the checked layer. Nothing else reaches here: a constant is a literal and
    // nothing but a literal is a constant.
    let origin = expr.origin();
    let Ok(crate::ir::Expr::Lit(lit)) = crate::checked::erase_expr(expr) else {
        return original;
    };
    CheckedExpr::const_at(lit, phase, origin)
}
