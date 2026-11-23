mod common;

use common::{Running, assert_closed};
use std::io::{Read, Write};

#[test]
fn negotiates_bolt_5_and_closes_on_bad_magic() {
    let server = Running::start();
    let mut client = server.connect();
    // Hand-written proposals: 5.4 through 5.0, then three empty proposals.
    client
        .write_all(&[
            0x60, 0x60, 0xb0, 0x17, 0, 4, 4, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ])
        .unwrap();
    let mut agreed = [0; 4];
    client.read_exact(&mut agreed).unwrap();
    assert_eq!(
        agreed,
        [0, 0, 4, 5],
        "the highest mutually supported Bolt version must be selected"
    );
    drop(client);
    let mut bad = server.connect();
    bad.write_all(&[0; 20]).unwrap();
    assert_closed(&mut bad);
}

#[test]
fn no_common_version_sends_zero_and_closes() {
    let server = Running::start();
    let mut client = server.connect();
    client
        .write_all(&[
            0x60, 0x60, 0xb0, 0x17, 0, 0, 4, 4, 0, 0, 3, 3, 0, 0, 0, 0, 0, 0, 0, 0,
        ])
        .unwrap();
    let mut agreed = [0xff; 4];
    client.read_exact(&mut agreed).unwrap();
    assert_eq!(agreed, [0; 4]);
    assert_closed(&mut client);
}

#[test]
fn chooses_highest_across_proposals_and_intersects_minor_range() {
    let server = Running::start();
    let mut client = server.connect();
    client
        .write_all(&[
            0x60, 0x60, 0xb0, 0x17, 0, 0, 0, 5, 0, 3, 6, 5, 0, 0, 1, 5, 0, 0, 0, 0,
        ])
        .unwrap();
    let mut agreed = [0; 4];
    client.read_exact(&mut agreed).unwrap();
    assert_eq!(agreed, [0, 0, 4, 5]);
}
