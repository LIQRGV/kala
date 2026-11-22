//! Slice 7, item 00 (c), L1: `Plan::explain` output is bounded.
//!
//! Expected API: `kala_query::MAX_EXPLAIN_BYTES: usize = 1 << 20`. Output is
//! never longer than `MAX_EXPLAIN_BYTES + 64` bytes; when cut, it is cut at a
//! line boundary and the last line is exactly `  ... (plan truncated)\n`.

mod common;

use common::*;
use kala_core::{Schema, World};
use kala_query::{Access, CmpOp, MAX_EXPLAIN_BYTES, MAX_PLAN_DEPTH, Op};

const MARK: &str = "  ... (plan truncated)\n";

fn deep_filters(n: usize) -> kala_query::Plan {
    let mut op = Op::Unit;
    for _ in 0..n {
        op = filter(op, CmpOp::Eq, ti(1), ti(1));
    }
    plan(&["x"], op)
}

#[test]
fn constant_is_one_mib() {
    assert_eq!(MAX_EXPLAIN_BYTES, 1 << 20);
}

#[test]
fn a_very_deep_plan_is_cut_at_a_line_boundary_with_a_marker() {
    let n = MAX_PLAN_DEPTH - 96; // 4,000 nested filters; untruncated is tens of MB
    let p = deep_filters(n);
    let w = World::in_memory(Schema::new()).unwrap();
    let text = p.explain(&w);
    assert!(
        text.len() <= MAX_EXPLAIN_BYTES + 64,
        "explain produced {} bytes",
        text.len()
    );
    assert!(
        text.ends_with(MARK),
        "tail: {:?}",
        &text[text.len().saturating_sub(80)..]
    );
    // Everything before the marker is whole, original lines: a header, then
    // Filter lines whose indentation grows by exactly two per line.
    let body = &text[..text.len() - MARK.len()];
    assert!(body.ends_with('\n'), "cut must be at a line boundary");
    let lines: Vec<&str> = body.lines().collect();
    assert!(lines[0].starts_with("Find "), "{:?}", lines[0]);
    assert!(
        lines.len() > 100,
        "a meaningful prefix is kept, got {}",
        lines.len()
    );
    for (k, l) in lines.iter().enumerate().skip(1) {
        let want = 2 * k; // root op at indent 2
        let indent = l.len() - l.trim_start().len();
        assert_eq!(indent, want, "line {k}: {l:?}");
        assert!(l.trim_start().starts_with("Filter"), "line {k}: {l:?}");
    }
    assert!(lines.len() - 1 < n, "the plan really was cut");
}

#[test]
fn a_normal_plan_is_not_truncated() {
    let w = varrow();
    let p = one_col(
        "v",
        expand(tv("e"), "ally_of", tv("v"), at(0), Access::Scan),
    );
    let text = p.explain(&w);
    assert!(!text.contains("truncated"), "{text}");
    assert!(text.ends_with('\n'));
    assert!(text.len() < 1000);
}

#[test]
fn a_moderately_deep_plan_under_the_cap_is_complete() {
    // 200 filters: indentation sum ~ 40 KB, far under 1 MiB.
    let p = deep_filters(200);
    let w = World::in_memory(Schema::new()).unwrap();
    let text = p.explain(&w);
    assert!(!text.contains("truncated"));
    assert_eq!(
        text.lines()
            .filter(|l| l.trim_start().starts_with("Filter"))
            .count(),
        200
    );
    assert!(
        text.trim_end().ends_with("Unit"),
        "{:?}",
        text.lines().last()
    );
}
