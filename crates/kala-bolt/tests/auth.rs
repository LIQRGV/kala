#[path = "common/client.rs"]
mod client;
mod common;
use client::{ALLIES, Client, map, string};
use common::{Running, assert_closed};
use kala_bolt::auth::{AuthFailure, AuthToken, Authenticator, Credentials, Identity, NoAuth};
use std::sync::Arc;

#[test]
fn credential_formatting_is_redacted_and_no_auth_accepts_every_scheme() {
    let credentials = Credentials::new("fake-test-secret".into());
    assert_eq!(format!("{credentials}"), "<redacted>");
    assert_eq!(format!("{credentials:?}"), "<redacted>");
    for scheme in ["none", "basic", "future"] {
        let token = AuthToken {
            scheme: scheme.into(),
            principal: "tester".into(),
            credentials: credentials.clone(),
        };
        assert!(
            NoAuth
                .authenticate(&token, "127.0.0.1".parse().unwrap())
                .is_ok()
        );
        assert!(!format!("{token:?}").contains("fake-test-secret"));
    }
}

struct Reject;
impl Authenticator for Reject {
    fn authenticate(&self, _: &AuthToken, _: std::net::IpAddr) -> Result<Identity, AuthFailure> {
        Err(AuthFailure)
    }
}

#[test]
fn authentication_failure_closes_instead_of_allowing_reset_bypass() {
    let server = Running::with_auth(Arc::new(Reject));
    for minor in [0, 4] {
        let mut client = Client::connected(&server, minor);
        if minor == 0 {
            client.send(
                0x01,
                vec![map(&[
                    ("user_agent", string("test")),
                    ("scheme", string("none")),
                ])],
            );
        } else {
            client.hello();
            client.success();
            client.send(0x6a, vec![map(&[("scheme", string("none"))])]);
        }
        client.failure("Neo.ClientError.Security.Unauthorized");
        assert_closed(&mut client.stream);
    }
}

#[test]
fn run_and_reset_before_logon_never_authenticate_the_connection() {
    let server = Running::with_auth(Arc::new(Reject));
    for (tag, fields) in [
        (0x10, vec![string(ALLIES), map(&[]), map(&[])]),
        (0x0f, vec![]),
    ] {
        let mut client = Client::connected(&server, 4);
        client.hello();
        client.success();
        client.send(tag, fields);
        client.failure("Neo.ClientError.Request.Invalid");
        assert_closed(&mut client.stream);
    }
}
