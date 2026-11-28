//! World discovery through the same Bolt operations drivers use.
#[path = "common/client.rs"]
mod client;
mod common;

use client::{ALLIES, Client, map, string};
use common::{Running, VARROW};
use kala_bolt::ServerConfig;
use kala_bolt::packstream::Value;

fn on(db: &str) -> Value {
    map(&[("db", string(db))])
}

#[test]
fn discovers_a_new_world_by_name_on_a_running_server() {
    let dir = tempfile::tempdir().unwrap();
    let server = Running::in_dir(
        dir,
        ServerConfig {
            listen: vec!["127.0.0.1:0".parse().unwrap()],
            ..ServerConfig::default()
        },
    );
    let mut c = Client::ready(&server);
    kala_fixture::load_fixture_to(VARROW, &server.dir.path().join("new.kala")).unwrap();
    c.run(ALLIES, map(&[]), on("new"));
    assert_eq!(c.pull(-1).0.len(), 2);
    c.run("SHOW DATABASES YIELD name, default", map(&[]), on("system"));
    assert_eq!(c.pull(-1).0, vec![vec![string("new"), Value::Bool(false)]]);
    c.send(0x10, vec![string(ALLIES), map(&[]), map(&[])]);
    c.failure("Neo.ClientError.Database.DatabaseNotFound");
}

#[test]
fn show_databases_discovers_only_eligible_files_and_keeps_the_startup_default() {
    let server = Running::start();
    let mut c = Client::ready(&server);
    let dir = server.dir.path();
    std::fs::copy(dir.join("varrow.kala"), dir.join("new.kala")).unwrap();
    std::fs::create_dir(dir.join("directory.kala")).unwrap();
    std::fs::write(dir.join("Bad.name.kala"), []).unwrap();
    std::fs::write(dir.join("system.kala"), []).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.join("varrow.kala"), dir.join("link.kala")).unwrap();

    assert_eq!(
        c.query("SHOW DATABASES YIELD name, default"),
        vec![
            vec![string("new"), Value::Bool(false)],
            vec![string("varrow"), Value::Bool(true)],
        ]
    );
}

#[test]
fn a_half_created_world_can_be_opened_when_the_file_is_complete() {
    let server = Running::start();
    let mut c = Client::ready(&server);
    let file = server.dir.path().join("partial.kala");
    std::fs::write(&file, b"K").unwrap();
    c.send(0x10, vec![string(ALLIES), map(&[]), on("partial")]);
    c.failure("Neo.DatabaseError.General.UnknownError");
    c.reset();
    std::fs::copy(server.dir.path().join("varrow.kala"), &file).unwrap();
    c.run(ALLIES, map(&[]), on("partial"));
    assert_eq!(c.pull(-1).0.len(), 2);
    // Entries remain registered even if their files subsequently disappear.
    std::fs::remove_file(file).unwrap();
    assert_eq!(
        c.query("SHOW DATABASE partial YIELD name"),
        vec![vec![string("partial")]]
    );
    c.send(0x10, vec![string(ALLIES), map(&[]), on("partial")]);
    c.failure("Neo.DatabaseError.General.UnknownError");
}

#[test]
fn simultaneous_sessions_can_read_the_same_new_world() {
    let server = Running::start();
    let clients: Vec<_> = (0..4).map(|_| Client::ready(&server)).collect();
    let dir = server.dir.path();
    std::fs::copy(dir.join("varrow.kala"), dir.join("new.kala")).unwrap();
    let start = std::sync::Barrier::new(clients.len());
    std::thread::scope(|scope| {
        for mut c in clients {
            let start = &start;
            scope.spawn(move || {
                start.wait();
                c.run(ALLIES, map(&[]), on("new"));
                assert_eq!(c.pull(-1).0.len(), 2);
            });
        }
    });
}
