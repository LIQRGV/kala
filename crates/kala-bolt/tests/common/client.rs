#![allow(dead_code)]
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;

use crate::common::{Running, handshake};
use kala_bolt::packstream::{Value, decode, encode};

pub fn map(entries: &[(&str, Value)]) -> Value {
    Value::Map(
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect(),
    )
}
pub fn string(s: &str) -> Value {
    Value::String(s.into())
}
pub fn structure(value: Value, tag: u8) -> Vec<Value> {
    match value {
        Value::Structure(actual, fields) => {
            assert_eq!(actual, tag, "unexpected message tag; fields={fields:?}");
            fields
        }
        other => panic!("expected structure, got {other:?}"),
    }
}
pub fn metadata(value: Value, tag: u8) -> BTreeMap<String, Value> {
    let fields = structure(value, tag);
    assert_eq!(fields.len(), 1);
    match fields.into_iter().next().unwrap() {
        Value::Map(map) => map,
        other => panic!("expected metadata map, got {other:?}"),
    }
}

pub struct Client {
    pub stream: TcpStream,
}
impl Client {
    pub fn connected(server: &Running, minor: u8) -> Self {
        let mut stream = server.connect();
        handshake(&mut stream, minor);
        Self { stream }
    }
    pub fn ready(server: &Running) -> Self {
        let mut client = Self::connected(server, 4);
        client.hello();
        client.success();
        client.send(0x6a, vec![map(&[("scheme", string("none"))])]);
        client.success();
        client
    }
    pub fn hello(&mut self) {
        // Independent, hand-written HELLO bytes rather than our encoder.
        self.raw(&[
            0xb1, 0x01, 0xa1, 0x8a, b'u', b's', b'e', b'r', b'_', b'a', b'g', b'e', b'n', b't',
            0x84, b't', b'e', b's', b't',
        ]);
    }
    pub fn raw(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(u16::MAX as usize) {
            self.stream
                .write_all(&(chunk.len() as u16).to_be_bytes())
                .unwrap();
            self.stream.write_all(chunk).unwrap();
        }
        self.stream.write_all(&[0, 0]).unwrap();
    }
    pub fn send(&mut self, tag: u8, fields: Vec<Value>) {
        self.raw(&encode(&Value::Structure(tag, fields)).unwrap());
    }
    pub fn recv(&mut self) -> Value {
        let mut bytes = vec![];
        loop {
            let mut header = [0; 2];
            self.stream.read_exact(&mut header).unwrap();
            let len = u16::from_be_bytes(header) as usize;
            if len == 0 {
                if bytes.is_empty() {
                    continue;
                }
                break;
            }
            let start = bytes.len();
            assert!(
                start + len <= 1024 * 1024,
                "server emitted oversized message"
            );
            bytes.resize(start + len, 0);
            self.stream.read_exact(&mut bytes[start..]).unwrap();
        }
        decode(&bytes).unwrap()
    }
    pub fn success(&mut self) -> BTreeMap<String, Value> {
        metadata(self.recv(), 0x70)
    }
    pub fn failure(&mut self, code: &str) -> BTreeMap<String, Value> {
        let meta = metadata(self.recv(), 0x7f);
        assert_eq!(meta.get("code"), Some(&string(code)));
        meta
    }
    pub fn reset(&mut self) {
        self.send(0x0f, vec![]);
        self.success();
    }
    pub fn run(&mut self, query: &str, params: Value, extra: Value) -> BTreeMap<String, Value> {
        self.send(0x10, vec![string(query), params, extra]);
        self.success()
    }
    pub fn run_empty(&mut self, query: &str) -> BTreeMap<String, Value> {
        self.run(query, map(&[]), map(&[]))
    }
    pub fn pull(&mut self, n: i64) -> (Vec<Vec<Value>>, BTreeMap<String, Value>) {
        self.send(
            0x3f,
            vec![map(&[
                ("n", Value::Integer(n)),
                ("qid", Value::Integer(-1)),
            ])],
        );
        self.records()
    }
    pub fn records(&mut self) -> (Vec<Vec<Value>>, BTreeMap<String, Value>) {
        let mut rows = vec![];
        loop {
            let message = self.recv();
            if let Value::Structure(0x71, fields) = &message {
                assert_eq!(fields.len(), 1);
                let Value::List(values) = &fields[0] else {
                    panic!("record must be a list")
                };
                rows.push(values.clone());
            } else {
                return (rows, metadata(message, 0x70));
            }
        }
    }
    pub fn query(&mut self, query: &str) -> Vec<Vec<Value>> {
        self.run_empty(query);
        self.pull(-1).0
    }
}

pub const ALLIES: &str = "// kala:datalog\n[:find ?a :at -210 :where [:edwin :ally_of ?a]]";
pub const NAMES: &str = "// kala:datalog\n[:find ?n :at -210 :where [?e :name ?n]]";
