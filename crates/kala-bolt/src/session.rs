//! One connection's Bolt state. Authentication failures are terminal.
use crate::{
    auth::{AuthToken, Authenticator, Credentials, Identity},
    chunk,
    error::{Failure, INVALID},
    handshake,
    message::{Map, Message, success},
    packstream::{self, Value},
    query,
    registry::Registry,
    server::ServerConfig,
};
use std::io;
use std::net::{IpAddr, TcpStream};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Connected,
    Authentication,
    Ready,
    Streaming,
    Failed,
}
// Once any bytes might have been written, a transport error is terminal:
// appending FAILURE could otherwise corrupt a partially transmitted chunk.
enum RequestError {
    Client(Failure),
    Transport(io::Error),
}
impl From<Failure> for RequestError {
    fn from(failure: Failure) -> Self {
        Self::Client(failure)
    }
}
struct Session<'a> {
    state: State,
    minor: u8,
    authenticated: bool,
    identity: Option<Identity>,
    registry: &'a Registry,
    auth: &'a dyn Authenticator,
    config: &'a ServerConfig,
    id: u64,
    peer: IpAddr,
    output: Option<query::Output>,
    db: String,
    started: Instant,
}
pub(crate) fn serve(
    mut stream: TcpStream,
    registry: &Registry,
    auth: &dyn Authenticator,
    config: &ServerConfig,
    id: u64,
    stop: &AtomicBool,
) -> io::Result<()> {
    let peer = stream.peer_addr()?.ip();
    stream.set_write_timeout(Some(config.write_timeout))?;
    let Some(minor) = handshake::negotiate(&mut stream, config.handshake_timeout)? else {
        return Ok(());
    };
    eprintln!("event=bolt_negotiated connection_id={id} version=5.{minor}");
    let mut session = Session {
        state: State::Connected,
        minor,
        authenticated: false,
        identity: None,
        registry,
        auth,
        config,
        id,
        peer,
        output: None,
        db: String::new(),
        started: Instant::now(),
    };
    loop {
        let bytes =
            match chunk::read_message(&mut stream, config.max_message_bytes, config.idle_timeout) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                    let _ = session.send(
                        &mut stream,
                        &Failure::new(INVALID, "message size limit exceeded").value(),
                        Instant::now() + config.write_timeout,
                    );
                    return Ok(());
                }
                Err(e) => return Err(e),
            };
        let value =
            packstream::decode_with_limits(&bytes, config.max_message_bytes, config.max_depth);
        // Input may carry credentials. Drop the original bytes before dispatch,
        // never format them or the decoded request, even on malformed input.
        drop(bytes);
        let value = match value {
            Ok(v) => v,
            Err(_) => {
                let _ = session.send(
                    &mut stream,
                    &Failure::new(INVALID, "malformed PackStream message").value(),
                    Instant::now() + config.write_timeout,
                );
                return Ok(());
            }
        };
        let message = Message::parse(value);
        let deadline = Instant::now() + config.write_timeout;
        if matches!(message, Ok(Message::Goodbye)) {
            return Ok(());
        }
        if session.state == State::Failed && !matches!(message, Ok(Message::Reset)) {
            session.send(&mut stream, &Value::Structure(0x7e, vec![]), deadline)?;
            continue;
        }
        let result = match message {
            Ok(msg) => session.handle(&mut stream, msg, stop),
            Err(e) => Err(RequestError::Client(e)),
        };
        if let Err(error) = result {
            let failure = match error {
                RequestError::Transport(error) => return Err(error),
                RequestError::Client(failure) => failure,
            };
            eprintln!(
                "event=bolt_request_failed connection_id={id} code={}",
                failure.code
            );
            session.output = None;
            session.send(
                &mut stream,
                &failure.value(),
                Instant::now() + config.write_timeout,
            )?;
            if !session.authenticated {
                return Ok(());
            }
            session.state = State::Failed;
        }
    }
}
impl Session<'_> {
    fn send(&self, stream: &mut TcpStream, value: &Value, deadline: Instant) -> io::Result<()> {
        let bytes = packstream::encode_with_limits(
            value,
            self.config.max_message_bytes,
            self.config.max_depth,
        )
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "response size limit exceeded"))?;
        chunk::write_message(stream, &bytes, deadline)
    }
    fn authenticate(&mut self, mut map: Map) -> Result<(), Failure> {
        fn field(map: &mut Map, name: &str) -> Result<String, Failure> {
            match map.remove(name) {
                None => Ok(String::new()),
                Some(Value::String(s)) => Ok(s),
                _ => Err(Failure::invalid()),
            }
        }
        // The official Python driver's auth=None is an empty auth map.
        // Only absence of every auth field means none; incomplete basic
        // tokens must not accidentally select the local root/trust account.
        let scheme = if !["scheme", "principal", "credentials"]
            .iter()
            .any(|key| map.contains_key(*key))
        {
            "none".into()
        } else {
            field(&mut map, "scheme")?
        };
        let token = AuthToken {
            scheme,
            principal: field(&mut map, "principal")?,
            credentials: Credentials::new(field(&mut map, "credentials")?),
        };
        let identity = self.auth.authenticate(&token, self.peer).map_err(|_| {
            Failure::new(
                "Neo.ClientError.Security.Unauthorized",
                "authentication failed",
            )
        })?;
        eprintln!(
            "event=bolt_authenticated connection_id={} user={:?} client_ip={}",
            self.id,
            crate::error::escape(&identity.principal),
            self.peer
        );
        self.identity = Some(identity);
        Ok(())
    }
    fn handle(
        &mut self,
        stream: &mut TcpStream,
        msg: Message,
        stop: &AtomicBool,
    ) -> Result<(), RequestError> {
        let response = match msg {
            Message::Hello(extra) if self.state == State::Connected => {
                if self.minor == 0 {
                    self.authenticate(extra)?;
                    self.authenticated = true;
                    self.state = State::Ready;
                } else {
                    self.state = State::Authentication;
                }
                // Drivers require an agent starting with `Neo4j/` and may
                // read its version for feature checks, so it names the Bolt-5
                // compatibility level, not Kala's own version (which is
                // `kala_version`; drivers ignore unknown keys).
                let mut hints = Map::new();
                // Lets pooled drivers retire connections before the idle
                // timeout closes them. Drivers also use it as a read timeout
                // while waiting for a response, so it is only sent for a
                // whole number of seconds (0 means "none" to some drivers).
                let secs = self.config.idle_timeout.as_secs();
                if secs >= 1 {
                    hints.insert(
                        "connection.recv_timeout_seconds".into(),
                        Value::Integer(i64::try_from(secs).unwrap_or(i64::MAX)),
                    );
                }
                success([
                    ("server", Value::String("Neo4j/5.0.0".into())),
                    (
                        "kala_version",
                        Value::String(env!("CARGO_PKG_VERSION").into()),
                    ),
                    ("connection_id", Value::String(format!("kala-{}", self.id))),
                    ("hints", Value::Map(hints)),
                ])
            }
            Message::Logon(extra) if self.minor >= 1 && self.state == State::Authentication => {
                self.authenticate(extra)?;
                self.authenticated = true;
                self.state = State::Ready;
                success([])
            }
            Message::Logoff if self.minor >= 1 && self.state == State::Ready => {
                self.authenticated = false;
                self.identity = None;
                self.state = State::Authentication;
                success([])
            }
            Message::Reset if self.authenticated => {
                self.output = None;
                self.state = State::Ready;
                success([])
            }
            Message::Telemetry if self.minor >= 4 && self.state == State::Ready => success([]),
            Message::Unsupported if self.authenticated => {
                return Err(Failure::new(INVALID, "not supported yet").into());
            }
            Message::Run {
                text,
                params,
                extra,
            } if self.state == State::Ready => {
                let requested = match extra.get("db") {
                    None => None,
                    Some(Value::String(s)) => Some(s.as_str()),
                    _ => return Err(Failure::invalid().into()),
                };
                let name = self.registry.name(requested)?;
                if !self
                    .identity
                    .as_ref()
                    .is_some_and(|identity| identity.permits_read(name))
                {
                    return Err(Failure::new(
                        crate::error::FORBIDDEN,
                        format!("access to database {name:?} is forbidden"),
                    )
                    .into());
                }
                let (db, world) = self.registry.get(Some(name))?;
                self.started = Instant::now();
                eprintln!("event=bolt_query_started connection_id={} db={db}", self.id);
                let output = query::run(
                    &world,
                    &text,
                    &params,
                    stop,
                    self.config.max_message_bytes,
                    self.config.max_depth,
                )?;
                let fields =
                    Value::List(output.fields.iter().cloned().map(Value::String).collect());
                eprintln!(
                    "event=bolt_query_completed connection_id={} rows={}",
                    self.id,
                    output.rows.len()
                );
                self.db = db;
                self.output = Some(output);
                self.state = State::Streaming;
                success([
                    ("fields", fields),
                    ("t_first", Value::Integer(elapsed(self.started))),
                    ("qid", Value::Integer(0)),
                ])
            }
            Message::Pull { n, qid, discard } if self.state == State::Streaming => {
                if qid != -1 && qid != 0 {
                    return Err(Failure::invalid().into());
                }
                let output = self.output.as_mut().ok_or_else(Failure::invalid)?;
                let count = if n == -1 {
                    output.rows.len()
                } else {
                    usize::try_from(n)
                        .unwrap_or(usize::MAX)
                        .min(output.rows.len())
                };
                let deadline = Instant::now() + self.config.write_timeout;
                for _ in 0..count {
                    if let Some(bytes) = output.rows.pop_front()
                        && !discard
                    {
                        chunk::write_message(stream, &bytes, deadline)
                            .map_err(RequestError::Transport)?;
                    }
                }
                if output.rows.is_empty() {
                    self.output = None;
                    self.state = State::Ready;
                    success([
                        ("t_last", Value::Integer(elapsed(self.started))),
                        ("type", Value::String("r".into())),
                        ("db", Value::String(self.db.clone())),
                    ])
                } else {
                    success([("has_more", Value::Bool(true))])
                }
            }
            _ => return Err(Failure::invalid().into()),
        };
        self.send(
            stream,
            &response,
            Instant::now() + self.config.write_timeout,
        )
        .map_err(RequestError::Transport)
    }
}
fn elapsed(start: Instant) -> i64 {
    i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX)
}
