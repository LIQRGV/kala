//! Apply a validated fixture plan through the native transaction API.
use std::collections::HashMap;

use kala_core::log::EntryId;
use kala_core::world::{Correction, Op, Txn, World};
use kala_core::{Echo, EntityId, Error, Value, ValueType};

use crate::error::{FixtureError, FixtureErrorKind, OpRef, err, invalid, store};
use crate::plan::{PFix, POp, PTxn, Plan, Subject, to_name, to_triple};
use crate::yaml::{Item, Scalar};

/// The largest integer every f64 holds exactly (2^53).
const MAX_EXACT_F64_INT: i64 = 1 << 53;

/// What a label names once its transaction committed.
#[derive(Clone, Copy, Debug)]
enum Labelled {
    /// The base entry its op stored, and its attribute's value type.
    Stored(EntryId, ValueType),
    /// Its op was a "0 changed" no-op.
    Nothing,
}

/// What one op did: how many entries it stored and, for a labelled write,
/// its label and attribute type.
type Outcome<'p> = (usize, Option<(&'p str, ValueType)>);

struct Runner<'p> {
    plan: &'p Plan,
    /// Every handle created so far (committed or staged in the current
    /// transaction): the loader builds the world from empty, so this is the
    /// world's handle table.
    handles: HashMap<String, EntityId>,
    /// Labels of committed transactions.
    labels: HashMap<String, Labelled>,
}

pub(super) fn run(plan: &Plan, world: &mut World) -> Result<(), FixtureError> {
    let mut r = Runner {
        plan,
        handles: HashMap::new(),
        labels: HashMap::new(),
    };
    let mut tx = world.begin();
    for (ty, hs) in &plan.entities {
        for h in hs {
            let id = tx.create_entity(ty, Some(h)).map_err(|e| store(None, e))?;
            r.handles.insert(h.clone(), id);
        }
    }
    tx.commit_quiet().map_err(|e| store(None, e))?;
    for t in &plan.txns {
        r.txn(world, t)?;
    }
    Ok(())
}

impl Runner<'_> {
    fn txn(&mut self, world: &mut World, t: &PTxn) -> Result<(), FixtureError> {
        let mut tx = world.begin();
        if let Some(d) = t.tick {
            tx.set_default_tick(d);
        }
        // Per op: the entries it stored and, for a labelled write, the label.
        let mut stored: Vec<Outcome<'_>> = Vec::with_capacity(t.ops.len());
        for (at, op) in &t.ops {
            let s = self.op(&mut tx, at, op)?;
            stored.push(s);
        }
        let last = t.ops.last().map(|(at, _)| at.clone());
        let res = tx.commit_quiet().map_err(|e| store(last.clone(), e))?;
        // Entries are listed in write order, `changed` per op.
        let mut next = res.entries.iter();
        for (changed, label) in stored {
            // The op's first entry is its base fact (a correction's entries
            // are skipped: labels name only writes).
            let mut mine = next.by_ref().take(changed);
            let first = mine.next().copied();
            mine.for_each(drop);
            let Some((l, vt)) = label else { continue };
            let named = match (changed, first) {
                (0, _) => Labelled::Nothing,
                (_, Some(e)) => Labelled::Stored(e, vt),
                (_, None) => {
                    return Err(invalid(
                        last,
                        "internal: the commit listed fewer entries than its writes stored",
                    ));
                }
            };
            self.labels.insert(l.to_owned(), named);
        }
        Ok(())
    }

    /// Runs one op; returns the number of entries it stored and its label.
    fn op<'p>(
        &mut self,
        tx: &mut Txn<'_>,
        at: &OpRef,
        op: &'p POp,
    ) -> Result<Outcome<'p>, FixtureError> {
        let pos = || Some(at.clone());
        match op {
            POp::Create { node_type, handle } => {
                let id = tx
                    .create_entity(node_type, Some(handle))
                    .map_err(|e| store(pos(), e))?;
                self.handles.insert(handle.clone(), id);
                Ok((0, None))
            }
            POp::Write {
                op,
                subject,
                attr,
                value,
                tick,
                precision,
                label,
            } => {
                let e = self.subject(tx, at, subject)?;
                let Some(&vt) = self.plan.attr_types.get(attr) else {
                    return Err(store(pos(), Error::UnknownAttribute(attr.clone())));
                };
                let v = self.value(tx, at, attr, vt, value)?;
                let res = match op {
                    Op::Assert => tx.assert_with_precision(e, attr, v, *tick, *precision),
                    Op::Retract => tx.retract_with_precision(e, attr, v, *tick, *precision),
                }
                .map_err(|e| store(pos(), e))?;
                Ok((res.changed, label.as_deref().map(|l| (l, vt))))
            }
            POp::Correct { label, fix } => self
                .correct(tx, at, label, fix)
                .map(|changed| (changed, None)),
        }
    }

    fn correct(
        &self,
        tx: &mut Txn<'_>,
        at: &OpRef,
        label: &str,
        fix: &PFix,
    ) -> Result<usize, FixtureError> {
        let pos = || Some(at.clone());
        let (entry, vt) = match self.labels.get(label) {
            Some(Labelled::Stored(entry, vt)) => (*entry, *vt),
            Some(Labelled::Nothing) => {
                return Err(invalid(
                    pos(),
                    format!(
                        "label `{}` names an op that stored nothing (0 changed)",
                        Echo(label)
                    ),
                ));
            }
            None if self.plan.labels.contains(label) => {
                return Err(invalid(
                    pos(),
                    format!(
                        "label `{}` is corrected before its transaction committed",
                        Echo(label)
                    ),
                ));
            }
            None => {
                return Err(err(pos(), FixtureErrorKind::UnknownLabel(label.to_owned())));
            }
        };
        let c = match fix {
            PFix::Delete => Correction::Delete,
            PFix::Replace {
                value,
                tick,
                op,
                precision,
            } => Correction::Replace {
                value: match value {
                    None => None,
                    Some(item) => Some(self.value(tx, at, "the replaced value", vt, item)?),
                },
                tick: *tick,
                op: *op,
                precision: *precision,
            },
        };
        let res = tx.correct(entry, c).map_err(|e| store(pos(), e))?;
        Ok(res.changed)
    }

    fn entity(&self, at: &OpRef, handle: &str) -> Result<EntityId, FixtureError> {
        self.handles.get(handle).copied().ok_or_else(|| {
            err(
                Some(at.clone()),
                FixtureErrorKind::UnknownHandle(handle.to_owned()),
            )
        })
    }

    fn edge(
        &self,
        tx: &Txn<'_>,
        at: &OpRef,
        (s, a, t): (&str, &str, &str),
    ) -> Result<EntityId, FixtureError> {
        let (source, target) = (self.entity(at, s)?, self.entity(at, t)?);
        tx.edge_id(source, a, target)
            .map(EntityId::from)
            .ok_or_else(|| {
                err(
                    Some(at.clone()),
                    FixtureErrorKind::UnknownEdge(format!("[{s}, {a}, {t}]")),
                )
            })
    }

    fn subject(&self, tx: &Txn<'_>, at: &OpRef, s: &Subject) -> Result<EntityId, FixtureError> {
        match s {
            Subject::Handle(h) => self.entity(at, h),
            Subject::Edge(s, a, t) => self.edge(tx, at, (s, a, t)),
        }
    }

    /// A value of type `vt` from the YAML item (a handle or an edge triple
    /// for `ref`).
    fn value(
        &self,
        tx: &Txn<'_>,
        at: &OpRef,
        attr: &str,
        vt: ValueType,
        item: &Item,
    ) -> Result<Value, FixtureError> {
        let bad = |want: &str, got: &str| {
            invalid(
                Some(at.clone()),
                format!("the value of `{}` must be {want}, not {got}", Echo(attr)),
            )
        };
        let s = match item {
            Item::Scalar(s) => s,
            Item::List(l) if vt == ValueType::Ref => {
                let (s, a, t) = to_triple(l).map_err(|m| invalid(Some(at.clone()), m))?;
                return Ok(Value::Ref(self.edge(tx, at, (&s, &a, &t))?));
            }
            Item::List(_) => return Err(bad(want(vt), "a list")),
        };
        Ok(match (vt, s) {
            (ValueType::Bool, Scalar::Bool(b)) => Value::Bool(*b),
            (ValueType::I64, Scalar::Int(i)) => match i64::try_from(*i) {
                Ok(i) => Value::I64(i),
                Err(_) => return Err(bad(want(vt), "an integer outside the i64 range")),
            },
            (ValueType::F64, Scalar::Float(f)) => Value::F64(*f),
            (ValueType::F64, Scalar::Int(i)) => match exact_f64(*i) {
                Some(f) => Value::F64(f),
                None => {
                    return Err(bad(
                        want(vt),
                        "an integer no f64 holds exactly (|n| > 2^53)",
                    ));
                }
            },
            (ValueType::Str, Scalar::Str(v)) => Value::Str(v.clone()),
            (ValueType::Ref, Scalar::Str(_)) => {
                let h = to_name(s, "a handle").map_err(|m| invalid(Some(at.clone()), m))?;
                Value::Ref(self.entity(at, &h)?)
            }
            (_, other) => return Err(bad(want(vt), other.describe())),
        })
    }
}

/// What a value of type `vt` is written as, for messages.
fn want(vt: ValueType) -> &'static str {
    match vt {
        ValueType::Bool => "true or false",
        ValueType::I64 => "an integer",
        ValueType::F64 => "a number",
        ValueType::Str => "a string",
        ValueType::Ref => "a handle or an edge triple",
    }
}

/// `i` as an f64 if every f64 conversion of it is exact (`|i| <= 2^53`).
fn exact_f64(i: i128) -> Option<f64> {
    let i = i64::try_from(i).ok()?;
    // Exact: integers up to 2^53 in magnitude are all representable, so this
    // conversion never rounds.
    (-MAX_EXACT_F64_INT..=MAX_EXACT_F64_INT)
        .contains(&i)
        .then_some(i as f64)
}
