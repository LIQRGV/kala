//! Adapter-owned metadata tables. Filtering/projection use the shared executor.
use crate::{
    auth::Identity,
    error::{EXECUTION, Failure},
    registry::Registry,
};
use kala_core::Value;
use kala_lang::{DatabaseSelection, ServerRequest};
use kala_query::{Datum, Limits};
fn string(s: &str) -> Datum {
    Value::Str(s.into()).into()
}
fn boolean(b: bool) -> Datum {
    Value::Bool(b).into()
}
pub(crate) fn rows(
    request: &ServerRequest,
    registry: &Registry,
    identity: &Identity,
    address: &str,
    stop: &std::sync::atomic::AtomicBool,
) -> Result<Vec<Datum>, Failure> {
    let limits = Limits::default();
    let mut steps = 0u64;
    let mut check = || -> Result<(), Failure> {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(Failure::new(EXECUTION, "metadata request cancelled"));
        }
        steps = steps.saturating_add(1);
        if steps > limits.max_steps {
            return Err(Failure::new(
                EXECUTION,
                "database metadata step budget exceeded",
            ));
        }
        Ok(())
    };
    check()?;
    if matches!(request, ServerRequest::Components) {
        return Ok(vec![Datum::List(vec![
            string("Kala"),
            Datum::List(vec![string("5.0.0")]),
            string("community"),
        ])]);
    }
    let ServerRequest::Databases(selection) = request else {
        unreachable!("components returned")
    };
    // Index captured grants once; a large registry must not multiply grant work.
    let mut granted = std::collections::HashSet::new();
    let mut wildcard = false;
    let mut system_admin = false;
    let mut grant_bytes = 0usize;
    for grant in &identity.grants {
        check()?;
        if let Some((privilege, target)) = grant.split_once(':') {
            wildcard |= target == "*";
            system_admin |= privilege == "admin" && (target == "*" || target == "system");
            grant_bytes = grant_bytes.saturating_add(64 + target.len());
            if grant_bytes > limits.max_bytes {
                return Err(Failure::new(
                    EXECUTION,
                    "database metadata memory budget exceeded",
                ));
            }
            granted.insert(target);
        }
    }
    let mut rows = Vec::new();
    let mut bytes = grant_bytes;
    for name in registry.names() {
        check()?;
        let visible = if name == "system" {
            system_admin
        } else {
            wildcard || granted.contains(name)
        };
        if !visible {
            continue;
        }
        let default = registry.default_name() == Some(name);
        let selected = match selection {
            DatabaseSelection::All => true,
            DatabaseSelection::Named(n) => n == name,
            DatabaseSelection::Default | DatabaseSelection::Home => default,
        };
        if !selected {
            continue;
        }
        bytes = bytes.saturating_add(2048 + name.len() + address.len());
        if bytes > limits.max_bytes || rows.len() >= limits.max_rows {
            return Err(Failure::new(
                EXECUTION,
                "database metadata resource limit exceeded",
            ));
        }
        rows.push(Datum::List(vec![
            string(name),
            string(if name == "system" {
                "system"
            } else {
                "standard"
            }),
            Datum::List(vec![]),
            string("read-only"),
            string(address),
            string("primary"),
            boolean(true),
            string("online"),
            string("online"),
            string(""),
            boolean(default),
            boolean(default),
            Datum::List(vec![]),
        ]));
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::SystemDb;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn cancelled_metadata_requests_stop_before_building_rows() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::open(dir.path(), None, SystemDb::Skip).unwrap();
        let identity = Identity {
            principal: "test".into(),
            host: "localhost".into(),
            grants: vec!["read:*".into()],
            generation: 0,
        };
        for request in [
            ServerRequest::Components,
            ServerRequest::Databases(DatabaseSelection::All),
        ] {
            let result = rows(
                &request,
                &registry,
                &identity,
                "127.0.0.1:7687",
                &AtomicBool::new(true),
            );
            assert!(
                result.is_err(),
                "cancelled metadata request must not build a table: {request:?}"
            );
        }
    }
}
