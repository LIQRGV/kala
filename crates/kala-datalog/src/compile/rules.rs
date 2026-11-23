//! The rule call graph: definitions grouped by name, calls resolved, the
//! graph's components in dependency order, stratification of negation, and
//! the head types and variable names definitions of one rule share.

use std::collections::{BTreeSet, HashMap, VecDeque};

use crate::ast::{Call, Item, Not, Or, Query, Rule};
use crate::error::{DatalogError, ErrorKind, show};

use super::Ty;
use super::clauses::{Clauses, body_vars};

/// All definitions of one rule name.
pub(super) struct RuleInfo<'q> {
    /// The definitions, in text order.
    pub(super) defs: Vec<&'q Rule>,
    /// The head length every definition and call shares.
    pub(super) arity: usize,
    /// The type of each head position, unified across definitions.
    pub(super) types: Vec<Option<Ty>>,
    /// The rules this one's bodies call (indices into the rule list).
    pub(super) callees: BTreeSet<usize>,
}

/// Groups the definitions by name (in order of first definition) and checks
/// that each name's definitions agree on the head length.
pub(super) fn group_rules(
    defs: &[Rule],
) -> Result<(Vec<RuleInfo<'_>>, HashMap<&str, usize>), DatalogError> {
    let mut rules: Vec<RuleInfo<'_>> = Vec::new();
    let mut by_name: HashMap<&str, usize> = HashMap::new();
    for d in defs {
        match by_name.get(d.name.as_str()) {
            Some(&i) => {
                let r = &mut rules[i];
                if r.arity != d.head.len() {
                    return Err(DatalogError::new(
                        ErrorKind::Arity,
                        Some(d.name_span),
                        format!(
                            "rule `{}` is defined with {} head variables here but {} before",
                            show(&d.name),
                            d.head.len(),
                            r.arity
                        ),
                    ));
                }
                r.defs.push(d);
            }
            None => {
                by_name.insert(d.name.as_str(), rules.len());
                rules.push(RuleInfo {
                    defs: vec![d],
                    arity: d.head.len(),
                    types: vec![None; d.head.len()],
                    callees: BTreeSet::new(),
                });
            }
        }
    }
    Ok((rules, by_name))
}

/// The rule `c` calls, checking it exists and takes that many arguments.
fn resolve_call(
    c: &Call,
    rules: &[RuleInfo<'_>],
    by_name: &HashMap<&str, usize>,
) -> Result<usize, DatalogError> {
    let Some(&i) = by_name.get(c.name.as_str()) else {
        return Err(DatalogError::new(
            ErrorKind::UnknownRule,
            Some(c.name_span),
            format!("no rule named `{}` in `:rules`", show(&c.name)),
        ));
    };
    let arity = rules[i].arity;
    if c.args.len() != arity {
        return Err(DatalogError::new(
            ErrorKind::Arity,
            Some(c.span),
            format!(
                "rule `{}` takes {arity} argument{}, not {}",
                show(&c.name),
                if arity == 1 { "" } else { "s" },
                c.args.len()
            ),
        ));
    }
    Ok(i)
}

/// Resolves every rule call, recording each rule's callees, and returns the
/// rules `:where` calls.
pub(super) fn link_calls(
    q: &Query,
    rules: &mut [RuleInfo<'_>],
    by_name: &HashMap<&str, usize>,
) -> Result<BTreeSet<usize>, DatalogError> {
    // A call inside a `not` is a dependency like any other: the rule
    // needs the callee's relation complete before it can negate it.
    for r in &q.rules {
        let caller = by_name[r.name.as_str()];
        for c in all_calls(&r.calls, &r.nots, &r.ors) {
            let callee = resolve_call(c, rules, by_name)?;
            rules[caller].callees.insert(callee);
        }
    }
    let mut where_callees = BTreeSet::new();
    for c in all_calls(&q.calls, &q.nots, &q.ors) {
        where_callees.insert(resolve_call(c, rules, by_name)?);
    }
    Ok(where_callees)
}

/// A strongly connected component of the rule call graph: rules that call
/// each other, directly or through other rules, evaluated together as one
/// `Fixpoint`.
///
/// This is where stratification belongs: negation (slice 7 item 4) must
/// not reach a rule of its own component through a negated call.
pub(super) struct Component {
    /// Its rules (indices into the rule list), in order of first definition.
    pub(super) rules: Vec<usize>,
    /// Whether it is recursive: more than one rule, or a rule calling itself.
    pub(super) recursive: bool,
}

/// The call graph's components in dependency order (every component after
/// the components it calls; otherwise in order of first definition), every
/// rule in exactly one. A cycle, even among rules `:where` never calls, is
/// recursion: its rules share a component.
pub(super) fn components(rules: &[RuleInfo<'_>]) -> Vec<Component> {
    let n = rules.len();
    // At most 64 rules: a reachability matrix and quadratic walks are cheap.
    let reach: Vec<BTreeSet<usize>> = (0..n)
        .map(|i| reachable(rules, &rules[i].callees))
        .collect();
    let mut comp_of = vec![usize::MAX; n];
    let mut comps: Vec<Component> = Vec::new();
    for i in 0..n {
        if comp_of[i] != usize::MAX {
            continue;
        }
        let members: Vec<usize> = (i..n)
            .filter(|&j| j == i || (reach[i].contains(&j) && reach[j].contains(&i)))
            .collect();
        for &j in &members {
            comp_of[j] = comps.len();
        }
        comps.push(Component {
            recursive: reach[i].contains(&i),
            rules: members,
        });
    }
    let mut done = vec![false; comps.len()];
    let mut order = Vec::with_capacity(comps.len());
    while order.len() < comps.len() {
        let next = (0..comps.len())
            .find(|&c| {
                !done[c]
                    && comps[c].rules.iter().all(|&r| {
                        rules[r]
                            .callees
                            .iter()
                            .all(|&k| comp_of[k] == c || done[comp_of[k]])
                    })
            })
            .expect("invariant: the component graph is acyclic");
        done[next] = true;
        order.push(next);
    }
    let mut slots: Vec<Option<Component>> = comps.into_iter().map(Some).collect();
    order
        .into_iter()
        .map(|c| {
            slots[c]
                .take()
                .expect("invariant: each component is ordered once")
        })
        .collect()
}

/// The rules reachable from `roots` through calls.
pub(super) fn reachable(rules: &[RuleInfo<'_>], roots: &BTreeSet<usize>) -> BTreeSet<usize> {
    let mut seen = BTreeSet::new();
    let mut todo: Vec<usize> = roots.iter().copied().collect();
    while let Some(i) = todo.pop() {
        if seen.insert(i) {
            todo.extend(rules[i].callees.iter().copied());
        }
    }
    seen
}

/// Every rule call of a body, those inside its negations (at any depth)
/// included.
fn all_calls<'q>(calls: &'q [Call], nots: &'q [Not], ors: &'q [Or]) -> Vec<&'q Call> {
    let mut out: Vec<&Call> = calls.iter().collect();
    for n in nots {
        out.extend(all_calls(&n.calls, &n.nots, &n.ors));
    }
    for o in ors {
        for b in &o.branches {
            out.extend(all_calls(&b.calls, &b.nots, &b.ors));
        }
    }
    out
}
fn negated_calls(c: Clauses<'_>) -> Vec<&Call> {
    let mut out = all_calls(&[], c.nots, &[]);
    for o in c.ors {
        for b in &o.branches {
            out.extend(negated_calls(Clauses::of_not(b)));
        }
    }
    out
}

/// Stratification: a rule's `not` may only reach rules of lower strata. A
/// call inside a `not` (at any depth) of a rule in the same component as
/// the rule would make the rule depend negatively on itself. Checked per
/// definition, in text order.
pub(super) fn stratify(
    defs: &[Rule],
    by_name: &HashMap<&str, usize>,
    comps: &[Component],
) -> Result<(), DatalogError> {
    let mut comp_of: HashMap<usize, usize> = HashMap::new();
    for (ci, c) in comps.iter().enumerate() {
        for &r in &c.rules {
            comp_of.insert(r, ci);
        }
    }
    for d in defs {
        let me = comp_of[&by_name[d.name.as_str()]];
        let negated = negated_calls(Clauses::of_rule(d));
        if let Some(c) = negated
            .into_iter()
            .find(|c| comp_of[&by_name[c.name.as_str()]] == me)
        {
            return Err(DatalogError::new(
                ErrorKind::Stratification,
                Some(c.span),
                format!(
                    "rule `{}` negates `{}`, which depends on `{}` itself: negation through \
                     recursion can't be stratified",
                    show(&d.name),
                    show(&c.name),
                    show(&d.name)
                ),
            ));
        }
    }
    Ok(())
}

/// Propagate a recursive component's known types along callee-position to
/// caller-head-position edges. A body's patterns/functions provide the seeds;
/// rule calls are the only way another position gains a type. Each position is
/// queued at most once, so propagation is linear in positions and arguments.
/// Conflicts are left to `rule_defs`, which preserves the existing diagnostics.
pub(super) fn propagate_head_types(
    comp: &Component,
    rules: &mut [RuleInfo<'_>],
    by_name: &HashMap<&str, usize>,
) {
    let mut offsets = vec![None; rules.len()];
    let mut types = Vec::new();
    for &ri in &comp.rules {
        offsets[ri] = Some(types.len());
        types.extend_from_slice(&rules[ri].types);
    }
    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); types.len()];
    for &ri in &comp.rules {
        let target = offsets[ri].expect("invariant: component member has an offset");
        for def in &rules[ri].defs {
            let heads: HashMap<&str, usize> = def
                .head
                .iter()
                .enumerate()
                .map(|(i, h)| (h.name.as_str(), target + i))
                .collect();
            // Every head is bound positively (checked by rule_defs), so a
            // same-named use inside a negation is shared with the outer body.
            for call in all_calls(&def.calls, &def.nots, &def.ors) {
                let callee = by_name[call.name.as_str()];
                let Some(source) = offsets[callee] else {
                    continue;
                };
                for (i, arg) in call.args.iter().enumerate() {
                    if let Item::Var(s) = arg
                        && let Some(&to) = heads.get(s.name.as_str())
                    {
                        edges[source + i].push(to);
                    }
                }
            }
        }
    }
    let mut queue: VecDeque<usize> = types
        .iter()
        .enumerate()
        .filter_map(|(i, t)| t.map(|_| i))
        .collect();
    while let Some(from) = queue.pop_front() {
        for &to in &edges[from] {
            if types[to].is_none() {
                types[to] = types[from];
                queue.push_back(to);
            }
        }
    }
    for &ri in &comp.rules {
        let offset = offsets[ri].expect("invariant: component member has an offset");
        let arity = rules[ri].arity;
        rules[ri]
            .types
            .copy_from_slice(&types[offset..offset + arity]);
    }
}

/// The variable renames of definition number `k` (0-based) of its rule,
/// whose plan columns are named `canon` (the first definition's head).
/// Head variable `i` becomes `canon[i]`; a body variable that happens to be
/// named like a column it isn't gets a name no query can write (`?a'1`:
/// `'` is not a name character), so it can't be confused with the column.
pub(super) fn renames(def: &Rule, canon: &[String], k: usize) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if k == 0 {
        return out;
    }
    for (h, c) in def.head.iter().zip(canon) {
        out.insert(h.name.clone(), c.clone());
    }
    let mut vars: Vec<&str> = Vec::new();
    body_vars(Clauses::of_rule(def), &mut vars);
    for v in vars {
        if !out.contains_key(v) && canon.iter().any(|c| c == v) {
            out.insert(v.to_owned(), format!("{v}'{k}"));
        }
    }
    out
}
