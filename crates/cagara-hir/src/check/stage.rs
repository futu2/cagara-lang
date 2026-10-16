//! Typed stage nodes: what each query-stage application read, and what it
//! produced.
//!
//! A query stage is a *function value* in this language — `prefix "u_"` has the
//! type `query r -> query (keyMap m r)` and the pipeline operator applies it —
//! so the stage's input row is not an argument the application can see. It is
//! the signature's own parameter, bound by the surrounding inference. That is
//! what makes the boundary explicit and checkable: **the stage value's type
//! says which row it reads and which row it produces**, and the operation it
//! applies is the one its definition declared.
//!
//! This module records one node per stage application, where the operation is
//! read and the signature is instantiated. The node keeps the stage's own
//! input row, its witness, and its output row — never a row recovered later by
//! inspecting a composed result. Adjacent nodes therefore chain: each stage's
//! input row is the previous stage's output row, which is the property the
//! two-key-stage defect violated.

use super::*;

/// The **stable identity** of one checked node, independent of where it was
/// written.
///
/// A node's span is where the *user* wrote it, and it stops being an identity
/// the moment a definition is inlined: the same body is instantiated at two use
/// sites, and the stage applications inside it are the same nodes at both —
/// `helper = q => q & prefix "h_"` applied twice applies one `prefix` twice.
/// Two instantiations would collide on `(module, span)` and be told apart by
/// nothing else, while a span-interval test silently matched whichever record
/// happened to enclose the search span.
///
/// `definition` is the definition the application sits in — the `(module, def)`
/// pair the checker was elaborating when it recorded the stage — and `expr_id`
/// is the `ExprId` of the application node inside that definition's AST. An
/// `ExprId` is unique per definition and is assigned by the parser, so the triple
/// names one node of one definition and nothing else.
///
/// Deliberately **crate-private**: it is an internal handle between the checker
/// and the lowering, and exposing it would publish a numbering (arena
/// positions, parser ids) that a later refactor is free to change.
///
/// A `NodeRef` survives saturation, inlining and substitution because it is
/// *not* derived from the term: the checker stamps it on the stage application
/// it recorded, the bridge carries it on the term node the application reduced
/// to, and substitution moves terms around without ever rewriting the stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeRef {
    /// The module the definition was written in.
    pub definition_module: usize,
    /// The source-order position of that definition within its module.
    pub definition: usize,
    /// The `ExprId` of the stage-application node inside that definition.
    pub expr_id: u32,
}

impl NodeRef {
    pub fn new(definition_module: usize, definition: usize, expr_id: u32) -> Self {
        NodeRef {
            definition_module,
            definition,
            expr_id,
        }
    }
}

/// The query stage a definition *is*, when its body is a `primitive`.
///
/// The kind travels in the definition's [`Scheme`], so a use site can name the
/// operation it applied without reading it back out of a composed result type.
/// A helper is not a stage: its scheme carries `None`, and the stages it
/// applies are the nodes recorded in its own body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowStage {
    Where,
    Select,
    Update,
    Omit,
    Prefix,
    Suffix,
    MapValue,
    Agg,
    Order,
    Limit,
    Offset,
    Distinct,
}

impl RowStage {
    /// The stage a primitive denotes, if it is a query stage.
    ///
    /// The relational primitives that are *not* stages — `table` (no input row
    /// at all), the joins and set operations (their remaining type is not
    /// `query input -> query output`), `merge`, and the expression-level
    /// primitives — are deliberately absent: a stage node is only meaningful
    /// for an operation that reads one row and produces one.
    pub(crate) fn of_prim(p: Prim) -> Option<RowStage> {
        use Prim::*;
        Some(match p {
            Where => RowStage::Where,
            Select => RowStage::Select,
            Update => RowStage::Update,
            Omit => RowStage::Omit,
            Prefix => RowStage::Prefix,
            Suffix => RowStage::Suffix,
            MapValue => RowStage::MapValue,
            AggStage => RowStage::Agg,
            Order => RowStage::Order,
            Limit => RowStage::Limit,
            Offset => RowStage::Offset,
            Distinct => RowStage::Distinct,
            Table | Merge | In | Join(_) | Set(_) | Group | Asc | Desc | Rows
            | UnboundedPreceding | UnboundedFollowing | CurrentRow | Preceding | Following => {
                return None
            }
        })
    }

    /// Does this stage's output row follow from its input row and its witness
    /// alone?
    ///
    /// The four that do are the row *rewrites*: a rename renames, a removal
    /// removes, a wrapper wraps. A projection or an aggregate builds its row
    /// from expressions the stage was given, so its output is not a function of
    /// its input and there is nothing to recompute.
    fn recomputes(self) -> bool {
        matches!(
            self,
            RowStage::Prefix | RowStage::Suffix | RowStage::Omit | RowStage::MapValue
        )
    }
}

/// A stage application recorded while checking one definition.
///
/// The rows are `Ty`s because they may still be open at the moment the stage is
/// applied — a helper's input row is its parameter — and are closed into
/// [`RowStageView`]s once the definition is finished and its rows are solved.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PendingRowStage {
    pub(crate) kind: RowStage,
    /// The `ExprId` of the application, so a consumer can find the node.
    pub(crate) site: u32,
    /// The stable identity of the application: which definition applied it, and
    /// which node inside that definition. This is what a later pass matches on
    /// instead of comparing spans.
    pub(crate) node: NodeRef,
    pub(crate) span: Span,
    /// The operation's witness — the affix, the removed label, or the wrapper
    /// name — as a type: the `Sym` the use site bound the signature's label to.
    pub(crate) label: Ty,
    /// The row the stage reads: the signature's own parameter row.
    pub(crate) input: Ty,
    /// The row the stage produces: the signature's own result row.
    pub(crate) output: Ty,
}

/// A stage application as a later phase sees it: closed rows only.
///
/// `input`/`output` are `None` when the row is still open (a helper's stage
/// applied to a parameter), which is different from a row with no columns.
#[derive(Debug, Clone, PartialEq)]
pub struct RowStageView {
    pub kind: RowStage,
    pub site: u32,
    /// The stable identity of the application; see [`PendingRowStage::node`].
    pub node: NodeRef,
    pub span: Span,
    pub input: Option<Vec<(String, ScalarType)>>,
    pub output: Option<Vec<(String, ScalarType)>>,
}

impl<'w> Checker<'w> {
    /// The stage the callee expression denotes, when it is a stage definition.
    ///
    /// Read from the *callee's scheme*, which is where the operation is owned:
    /// `prefix`'s scheme says it is a prefix stage wherever it is used, and a
    /// helper's says nothing (so a helper's use is not a stage application —
    /// the stages it applies were recorded in its own body).
    pub(crate) fn callee_row_stage(&self, f: &ast::Expr) -> Option<RowStage> {
        let Some(Resolved::Def((m, i))) = self.resolutions.get(&(self.module, f.id)) else {
            return None;
        };
        let (m, i) = (*m, *i);
        let scheme = if m == self.module {
            self.schemes.get(&(m, i))?
        } else {
            self.env.schemes.get(&m)?.get(&(m, i))?
        };
        scheme.stage
    }

    /// Record one stage application.
    ///
    /// Called where the operation is read — the key or wrapper argument — so
    /// the node's rows are the *stage's own* signature instance
    /// (`query input -> query output`), not a row recovered from a composed
    /// result. `remaining` is the type the stage has left once the witness
    /// argument is consumed.
    pub(crate) fn record_row_stage(
        &mut self,
        kind: RowStage,
        site: u32,
        span: Span,
        label: Ty,
        remaining: &Ty,
    ) {
        let Ty::Fun(param, result) = self.resolve(remaining) else {
            return;
        };
        let (Ty::Con("query", inp), Ty::Con("query", out)) =
            (self.resolve(&param), self.resolve(&result))
        else {
            return;
        };
        // The stable identity of the application: the definition the checker is
        // elaborating right now, and the application node's `ExprId` inside it.
        // `active` is the definition being checked (`check_def` pushes it), so
        // this is the one place the pair is known together with the `ExprId`.
        //
        // `active` is non-empty for every stage application — `record_row_stage`
        // is reached from `infer_app`, which only runs inside a body, and the
        // body belongs to the definition `check_def` pushed. The `None` arm is
        // therefore unreachable, and it deliberately does **not** panic: a
        // checker that abandoned the walk on an impossible state would turn a
        // guard into the crash it exists to avoid. It gets an identity nothing
        // else can collide with — `usize::MAX` is not a definition index — so a
        // record made through it matches no record in any definition and is
        // reported by the stage check rather than mistaken for a real one.
        let node = match self.active.last() {
            Some(&(module, def)) => NodeRef::new(module, def, site),
            None => NodeRef::new(self.module, usize::MAX, site),
        };
        self.row_stages.push(PendingRowStage {
            kind,
            site,
            node,
            span,
            label,
            input: inp[0].clone(),
            output: out[0].clone(),
        });
    }

    /// Close the recorded stage nodes, checking that each operation really
    /// produced its output from its input.
    ///
    /// The check is the invariant the redesign exists for: a stage's output row
    /// is computed by that operation, exactly once, from the row the operation
    /// read. A rename that does not rename, a removal that does not remove, or a
    /// wrapper that does not wrap is a compiler bug, so it is an assertion
    /// rather than a diagnostic: a user program cannot cause it, and there is
    /// nothing to report against. The schema layer, by contrast, reports its
    /// rules as diagnostics because *those* can be violated by a program.
    pub(crate) fn finish_row_stages(&mut self) -> Vec<RowStageView> {
        let mut out = Vec::new();
        for s in std::mem::take(&mut self.row_stages) {
            let input = self.zonk(&s.input);
            let output = self.zonk(&s.output);
            let label = self.zonk(&s.label);
            if s.kind.recomputes() {
                // A compiled-in check, not a `debug_assert!`: this is the
                // invariant the redesign exists for, and a release build is
                // where a compiler bug is least likely to be noticed and most
                // likely to be shipped. It can only fire on a genuine
                // disagreement — `row_stage_agrees` answers `true` whenever the
                // label or either row is still open, so a merely unresolved
                // program cannot trip it.
                assert!(
                    row_stage_agrees(s.kind, &label, &input, &output),
                    "stage {:?} at {:?} did not produce its output row from its input row: \
                     label = {}, input = {}, output = {}",
                    s.kind,
                    s.span,
                    self.show(&label),
                    self.show(&input),
                    self.show(&output),
                );
            }
            out.push(RowStageView {
                kind: s.kind,
                site: s.site,
                node: s.node,
                span: s.span,
                input: self.closed_fields(&input),
                output: self.closed_fields(&output),
            });
        }
        out
    }

    /// The columns of a row, when it is closed. `None` means "not known yet",
    /// which is different from a row with no columns.
    fn closed_fields(&self, t: &Ty) -> Option<Vec<(String, ScalarType)>> {
        let (fields, tail) = self.flatten(t);
        if tail != Ty::Empty {
            return None;
        }
        Some(
            fields
                .into_iter()
                .map(|(name, ty)| (name, ty_to_scalar(&ty)))
                .collect(),
        )
    }
}

/// Does the operation reproduce the node's output row from its input row?
///
/// Only the row rewrites are recomputed, and only when both rows are closed and
/// the witness is known; anything else is "nothing to check" rather than a
/// failure, so an open helper never trips the assertion.
fn row_stage_agrees(kind: RowStage, label: &Ty, input: &Ty, output: &Ty) -> bool {
    let Ty::Sym(name) = label else {
        return true;
    };
    let expected = match kind {
        RowStage::Prefix => reduce_mapkey(&KeyMap::Prefix(name.clone()).to_ty(), input),
        RowStage::Suffix => reduce_mapkey(&KeyMap::Suffix(name.clone()).to_ty(), input),
        RowStage::Omit => reduce_removekey(label, input),
        RowStage::MapValue => reduce_mapvalues(label, input),
        _ => return true,
    };
    match expected {
        Some(row) => row == *output,
        None => true,
    }
}
