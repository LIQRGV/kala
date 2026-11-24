//! The plan printer: [`Plan::explain`], the IR's stable text form.

use std::fmt::Write as _;

use kala_core::{Tick, Value, World};

use super::{
    Access, Agg, AttrRef, CanonSource, CmpOp, Op, OpTerm, Plan, RelDef, Term, TickSource, Var,
};

/// The most bytes [`Plan::explain`] prints for the plan's lines. Once the
/// next line would pass it, the output ends with the line
/// `TRUNCATED_LINE` instead (so it is never longer than this plus that
/// line). A plan deep enough to need the cap prints quadratic indentation:
/// 4,000 nested operators would otherwise be ~32 MB of text.
pub const MAX_EXPLAIN_BYTES: usize = 1 << 20;

/// The last line of a [`Plan::explain`] output cut at [`MAX_EXPLAIN_BYTES`].
const TRUNCATED_LINE: &str = "  ... (plan truncated)\n";

impl Plan {
    /// Renders the plan as stable text: a header line
    /// (`Find ?a  in $who  at -210  canon 41`), then one line per operator,
    /// each child indented two spaces more than its parent. Every line ends
    /// with `\n`. Entity constants are shown with [`World::format_id`]; an
    /// ID unknown to `world` is still printed, never a panic.
    ///
    /// Bounded: whole lines are printed while they fit in
    /// [`MAX_EXPLAIN_BYTES`]; the first one that doesn't (and everything
    /// after it) is replaced by the line `  ... (plan truncated)`.
    pub fn explain(&self, world: &World) -> String {
        let mut out = String::from("Find ");
        let vars: Vec<String> = self.find.iter().map(var_text).collect();
        out.push_str(&vars.join(" "));
        if !self.params.is_empty() {
            out.push_str("  in");
            for p in &self.params {
                out.push(' ');
                out.push_str(&param_text(p));
            }
        }
        out.push_str("  at ");
        match &self.default_tick {
            TickSource::Default => out.push_str("latest"),
            other => out.push_str(&tick_text(other)),
        }
        out.push_str("  canon ");
        match &self.canon {
            CanonSource::Latest => out.push_str("latest"),
            CanonSource::Const(n) => {
                let _ = write!(out, "{n}");
            }
            CanonSource::Param(p) => out.push_str(&param_text(p)),
        }
        out.push('\n');
        if out.len() > MAX_EXPLAIN_BYTES {
            // Only a plan no front-end builds has a header this long.
            return TRUNCATED_LINE.to_owned();
        }
        explain_op(&self.root, world, 2, &mut out);
        out
    }
}

/// `?name`. A generated variable (a front-end's hidden one, named with a
/// leading NUL) prints as `?#` plus the rest of its name, and any control
/// character is escaped as `{:?}` would, so explain text never holds one.
fn var_text(v: &Var) -> String {
    let mut out = String::from("?");
    let name = match v.0.strip_prefix('\0') {
        Some(rest) => {
            out.push('#');
            rest
        }
        None => v.0.as_str(),
    };
    push_name(&mut out, name);
    out
}

/// `$name`, control characters escaped as in [`var_text`].
fn param_text(p: &str) -> String {
    let mut out = String::from("$");
    push_name(&mut out, p);
    out
}

/// `name` with control characters escaped, for `format!` arguments.
fn name_text(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    push_name(&mut out, name);
    out
}

/// Appends `name` with its control characters escaped (`\n`, `\u{1}`).
fn push_name(out: &mut String, name: &str) {
    for c in name.chars() {
        if c.is_control() {
            out.extend(c.escape_debug());
        } else {
            out.push(c);
        }
    }
}

/// A [`RowAction`](crate::RowAction) line without its newline:
/// `nodes ?c :CHARACTER @latest`, `edge [?c :present ?p] as ?r out @-240`,
/// `bind ?#1 = ?c.name@latest`, `select (… OR …)`.
fn row_text(action: &crate::RowAction, world: &World, out: &mut String) {
    use crate::RowAction;
    out.push_str("Row ");
    match action {
        RowAction::Nodes { var, labels, tick } => {
            out.push_str("nodes ");
            out.push_str(&var_text(var));
            if !labels.is_empty() {
                out.push(' ');
            }
            for label in labels {
                out.push(':');
                push_name(out, label);
            }
            out.push_str(" @");
            out.push_str(&tick_text(tick));
        }
        RowAction::Edge {
            source,
            target,
            edge,
            attr,
            direction,
            tick,
        } => {
            out.push_str("edge [");
            out.push_str(&var_text(source));
            out.push_str(" :");
            push_name(out, attr);
            out.push(' ');
            out.push_str(&var_text(target));
            out.push_str("] as ");
            out.push_str(&var_text(edge));
            out.push_str(match direction {
                crate::Orientation::Out => " out @",
                crate::Orientation::In => " in @",
                crate::Orientation::Either => " either @",
            });
            out.push_str(&tick_text(tick));
        }
        RowAction::Bind { var, expr } => {
            out.push_str("bind ");
            out.push_str(&var_text(var));
            out.push_str(" = ");
            expr_text(expr, world, 0, out);
        }
        RowAction::Select(expr) => {
            out.push_str("select ");
            expr_text(expr, world, 0, out);
        }
    }
}

/// An [`OutputSpec`](crate::OutputSpec) line without its newline:
/// `Output ?n ?r  distinct  order ?#1 desc, ?x asc  skip 1  limit $l`.
fn output_text(spec: &crate::OutputSpec, world: &World, out: &mut String) {
    out.push_str("Output");
    for v in &spec.vars {
        out.push(' ');
        out.push_str(&var_text(v));
    }
    if spec.distinct {
        out.push_str("  distinct");
    }
    for (i, (v, desc)) in spec.order.iter().enumerate() {
        out.push_str(if i == 0 { "  order " } else { ", " });
        out.push_str(&var_text(v));
        out.push_str(if *desc { " desc" } else { " asc" });
    }
    if let Some(t) = &spec.skip {
        out.push_str("  skip ");
        out.push_str(&term_text(t, world));
    }
    if let Some(t) = &spec.limit {
        out.push_str("  limit ");
        out.push_str(&term_text(t, world));
    }
}

/// Deeper expression levels than this print as `...`. Above the
/// executor's own nesting limit, so every runnable expression prints whole,
/// while an expression from any front-end can't overflow the stack here.
const MAX_EXPLAIN_EXPR_DEPTH: usize = 64;

/// Appends `e`: boolean connectives in parentheses (`(a OR b)`), a
/// comparison, `IS NULL` or `NOT` operand in parentheses when it is itself
/// one of those, so the text reads back unambiguously. Recursion is capped
/// at [`MAX_EXPLAIN_EXPR_DEPTH`]; it also stops once the line alone is past
/// [`MAX_EXPLAIN_BYTES`] (it is truncated when pushed anyway).
fn expr_text(e: &crate::Expr, world: &World, depth: usize, out: &mut String) {
    use crate::Expr;
    if depth >= MAX_EXPLAIN_EXPR_DEPTH || out.len() > MAX_EXPLAIN_BYTES {
        out.push_str("...");
        return;
    }
    let operand = |e: &Expr, out: &mut String| {
        if matches!(e, Expr::Compare(..) | Expr::IsNull(_) | Expr::Not(_)) {
            out.push('(');
            expr_text(e, world, depth + 1, out);
            out.push(')');
        } else {
            expr_text(e, world, depth + 1, out);
        }
    };
    match e {
        Expr::Var(v) => out.push_str(&var_text(v)),
        Expr::Value(v) => out.push_str(&value_text(v, world)),
        Expr::Null => out.push_str("null"),
        Expr::Param(p) => out.push_str(&param_text(p)),
        Expr::Property { entity, key, tick } => {
            out.push_str(&var_text(entity));
            out.push('.');
            push_name(out, key);
            out.push('@');
            out.push_str(&tick_text(tick));
        }
        Expr::Compare(op, a, b) => {
            operand(a, out);
            out.push(' ');
            // Row expressions are three-valued, Cypher-style: `<>` for `!=`.
            out.push_str(match op {
                CmpOp::Ne => "<>",
                op => op.symbol(),
            });
            out.push(' ');
            operand(b, out);
        }
        Expr::And(a, b) | Expr::Or(a, b) | Expr::Xor(a, b) => {
            out.push('(');
            expr_text(a, world, depth + 1, out);
            out.push_str(match e {
                Expr::And(..) => " AND ",
                Expr::Or(..) => " OR ",
                _ => " XOR ",
            });
            expr_text(b, world, depth + 1, out);
            out.push(')');
        }
        Expr::Not(x) => {
            out.push_str("NOT ");
            operand(x, out);
        }
        Expr::IsNull(x) => {
            operand(x, out);
            out.push_str(" IS NULL");
        }
    }
}

/// A tick source without the `@` prefix (`latest`, `-210`, `$t`, `?t`).
fn tick_text(t: &TickSource) -> String {
    match t {
        TickSource::Default => "latest".to_string(),
        TickSource::Const(Tick(n)) => n.to_string(),
        TickSource::Param(p) => param_text(p),
        TickSource::Var(v) => var_text(v),
    }
}

fn value_text(v: &Value, world: &World) -> String {
    match v {
        Value::Bool(b) => b.to_string(),
        Value::I64(n) => n.to_string(),
        Value::F64(f) => format!("{f:?}"),
        // `{:?}` on a str quotes it and escapes `"`, `\` and control chars
        // (but not `'`), which is the stable form the printer promises.
        Value::Str(s) => format!("{s:?}"),
        Value::Ref(id) => world.format_id(*id),
    }
}

fn term_text(t: &Term, world: &World) -> String {
    match t {
        Term::Var(v) => var_text(v),
        Term::Param(p) => param_text(p),
        Term::Const(v) => value_text(v, world),
        Term::Op(o) => op_text(*o).to_string(),
        Term::Precision(p) => format!(":{}", precision_text(*p)),
        Term::Any => "_".to_string(),
    }
}

fn op_text(op: kala_core::Op) -> &'static str {
    match op {
        kala_core::Op::Assert => ":assert",
        kala_core::Op::Retract => ":retract",
    }
}

fn access_text(a: Access) -> &'static str {
    match a {
        Access::E => "E",
        Access::R => "R",
        Access::V => "V",
        Access::Scan => "scan",
        Access::Handle => "handle",
    }
}

/// Prints `root` and everything under it, children in order, each child
/// indented two more spaces. Walked with an explicit stack, not recursion, so
/// a very deep plan can't overflow the native stack.
///
/// Each line is built in a reused buffer and appended only if it fits in
/// [`MAX_EXPLAIN_BYTES`]; otherwise [`TRUNCATED_LINE`] ends the output.
fn explain_op(root: &Op, world: &World, indent: usize, out: &mut String) {
    /// One line to print: an operator, or a `Fixpoint`'s `Define` line
    /// (not an operator: its children are the relation's rules).
    enum Item<'p> {
        Op(&'p Op),
        Define(&'p RelDef),
    }
    let mut todo = vec![(Item::Op(root), indent)];
    let mut line = String::new();
    let mut kids = Vec::new();
    while let Some((item, indent)) = todo.pop() {
        if !line.is_empty() && !push_line(out, &line) {
            return;
        }
        line.clear();
        for _ in 0..indent {
            line.push(' ');
        }
        let child = indent + 2;
        // Children are pushed last-first so they print first-first.
        match item {
            Item::Define(rel) => {
                define_line(rel, &mut line);
                todo.extend(rel.rules.iter().rev().map(|r| (Item::Op(r), child)));
            }
            // A `Fixpoint`'s relations (their `Define` lines) print before
            // its body.
            Item::Op(op @ Op::Fixpoint { rels, body }) => {
                op_line(op, world, &mut line);
                todo.push((Item::Op(body), child));
                todo.extend(rels.iter().rev().map(|r| (Item::Define(r), child)));
            }
            Item::Op(op) => {
                op_line(op, world, &mut line);
                kids.clear();
                op.children(&mut kids);
                todo.extend(kids.iter().rev().map(|&k| (Item::Op(k), child)));
            }
        }
    }
    if !line.is_empty() {
        push_line(out, &line);
    }
}

/// Appends a `Fixpoint` relation's line: `Define name ?a ?b`.
fn define_line(rel: &RelDef, out: &mut String) {
    out.push_str("Define ");
    push_name(out, &rel.name);
    for v in &rel.vars {
        out.push(' ');
        out.push_str(&var_text(v));
    }
    out.push('\n');
}

/// Appends `op`'s own line (its children are printed by the caller).
fn op_line(op: &Op, world: &World, out: &mut String) {
    match op {
        Op::Row { action, .. } => row_text(action, world, out),
        Op::Output { spec, .. } => output_text(spec, world, out),
        Op::Unit
        | Op::Join { .. }
        | Op::Union { .. }
        | Op::Not { .. }
        | Op::Difference { .. }
        | Op::Intersect { .. }
        | Op::Fixpoint { .. } => out.push_str(op.name()),
        Op::NodeScan { var, node_type, .. } => {
            let _ = write!(out, "NodeScan {} {}", var_text(var), name_text(node_type));
        }
        Op::NodeSeek { var, entity, .. } => {
            let seek = term_text(entity, world);
            let _ = write!(out, "NodeSeek {} = {seek}", var_text(var));
        }
        Op::Expand {
            entity,
            attr,
            value,
            tick,
            access,
            ..
        } => {
            let via = access_text(*access);
            let _ = match attr {
                AttrRef::Named(name) => {
                    let fact = fact_text(entity, name, value, world);
                    write!(out, "Expand [{fact}] @{} via {via}", tick_text(tick))
                }
                // The handle table has no history, so its tick is not shown.
                AttrRef::Handle => {
                    let fact = fact_text(entity, "kala_handle", value, world);
                    write!(out, "Expand [{fact}] via {via}")
                }
            };
        }
        Op::Filter { pred, .. } => {
            let (left, right) = (term_text(&pred.left, world), term_text(&pred.right, world));
            let _ = write!(out, "Filter {left} {} {right}", pred.op.symbol());
        }
        Op::Project { vars, .. } => {
            let vs: Vec<String> = vars.iter().map(var_text).collect();
            let _ = write!(out, "Project {}", vs.join(" "));
        }
        Op::History {
            entity,
            attr,
            value,
            tick,
            op,
            precision,
            ..
        } => {
            let fact = fact_text(entity, attr, value, world);
            let tick = term_text(tick, world);
            let op = op_term_text(op);
            let _ = write!(out, "History [{fact} {tick} {op}");
            if !matches!(precision, Term::Any) {
                let _ = write!(out, " {}", term_text(precision, world));
            }
            out.push(']');
        }
        Op::During {
            entity,
            attr,
            value,
            out: var,
            ..
        } => {
            let fact = fact_text(entity, attr, value, world);
            let _ = write!(out, "During [{fact}] -> {}", var_text(var));
        }
        Op::Apply {
            func,
            args,
            out: var,
            ..
        } => {
            let _ = write!(out, "Apply {}({})", func.name(), args_text(args, world));
            if let Some(v) = var {
                out.push_str(" -> ");
                out.push_str(&var_text(v));
            }
        }
        Op::Aggregate { group, aggs, .. } => aggregate_text(group, aggs, out),
        Op::Rel { name, args, .. } => {
            let _ = write!(out, "Rel {name}({})", args_text(args, world));
        }
    }
    out.push('\n');
}

/// The inside of a pattern's brackets: `?e :attr ?v`.
fn fact_text(entity: &Term, attr: &str, value: &Term, world: &World) -> String {
    format!(
        "{} :{} {}",
        term_text(entity, world),
        name_text(attr),
        term_text(value, world)
    )
}

/// A `History` op position: `_`, `?op`, `:assert` or `:retract`.
fn op_term_text(op: &OpTerm) -> String {
    match op {
        OpTerm::Any => "_".to_owned(),
        OpTerm::Var(v) => var_text(v),
        OpTerm::Const(o) => op_text(*o).to_owned(),
    }
}

/// Call arguments, comma-separated.
fn args_text(args: &[Term], world: &World) -> String {
    let args: Vec<String> = args.iter().map(|a| term_text(a, world)).collect();
    args.join(", ")
}

/// An `Aggregate` line without its newline:
/// `Aggregate ?g: count ?x -> ?n, max ?x -> ?m`.
fn aggregate_text(group: &[Var], aggs: &[Agg], out: &mut String) {
    out.push_str("Aggregate");
    for g in group {
        out.push(' ');
        out.push_str(&var_text(g));
    }
    out.push(':');
    for (n, a) in aggs.iter().enumerate() {
        out.push_str(if n == 0 { " " } else { ", " });
        let _ = write!(
            out,
            "{} {} -> {}",
            a.func.name(),
            var_text(&a.var),
            var_text(&a.out)
        );
    }
}

/// Appends `line` to `out` if the result stays within
/// [`MAX_EXPLAIN_BYTES`] and returns true; else appends [`TRUNCATED_LINE`]
/// and returns false (the output is complete).
fn push_line(out: &mut String, line: &str) -> bool {
    if out.len() + line.len() > MAX_EXPLAIN_BYTES {
        out.push_str(TRUNCATED_LINE);
        return false;
    }
    out.push_str(line);
    true
}

/// Stable language-neutral name of a tick precision.
pub fn precision_text(p: kala_core::Precision) -> &'static str {
    match p {
        kala_core::Precision::Exact => "exact",
        kala_core::Precision::Day => "day",
        kala_core::Precision::Month => "month",
        kala_core::Precision::Year => "year",
        kala_core::Precision::Decade => "decade",
        kala_core::Precision::Century => "century",
    }
}
