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
use std::ops::ControlFlow;
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
    advertised_address: String,
    output: Option<query::Output>,
    db: String,
    started: Instant,
}
/// What every connection of one server shares.
pub(crate) struct Shared<'a> {
    pub registry: &'a Registry,
    pub auth: &'a dyn Authenticator,
    pub config: &'a ServerConfig,
    /// Set at shutdown; cancels running queries.
    pub stop: &'a AtomicBool,
}
pub(crate) fn serve(mut stream: TcpStream, id: u64, shared: &Shared<'_>) -> io::Result<()> {
    let config = shared.config;
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
        registry: shared.registry,
        auth: shared.auth,
        config,
        id,
        peer,
        advertised_address: config
            .advertised_address
            .clone()
            .unwrap_or(stream.local_addr()?.to_string()),
        output: None,
        db: String::new(),
        started: Instant::now(),
    };
    loop {
        let Some(message) = session.receive(&mut stream)? else {
            return Ok(());
        };
        let deadline = Instant::now() + config.write_timeout;
        if matches!(message, Ok(Message::Goodbye)) {
            return Ok(());
        }
        if session.state == State::Failed && !matches!(message, Ok(Message::Reset)) {
            session.send(&mut stream, &Value::Structure(0x7e, vec![]), deadline)?;
            continue;
        }
        let result = match message {
            Ok(msg) => session.handle(&mut stream, msg, shared.stop),
            Err(e) => Err(RequestError::Client(e)),
        };
        if let Err(error) = result
            && session.fail(&mut stream, error)?.is_break()
        {
            return Ok(());
        }
    }
}
impl Session<'_> {
    /// Reads and parses the next request. `None` ends the connection: the
    /// message was over the size limit or not PackStream, and the client
    /// has been told so (if it is still listening).
    fn receive(&self, stream: &mut TcpStream) -> io::Result<Option<Result<Message, Failure>>> {
        let config = self.config;
        let bytes = match chunk::read_message(stream, config.max_message_bytes, config.idle_timeout)
        {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                let _ = self.send(
                    stream,
                    &Failure::new(INVALID, "message size limit exceeded").value(),
                    Instant::now() + config.write_timeout,
                );
                return Ok(None);
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
                let _ = self.send(
                    stream,
                    &Failure::new(INVALID, "malformed PackStream message").value(),
                    Instant::now() + config.write_timeout,
                );
                return Ok(None);
            }
        };
        Ok(Some(Message::parse(value)))
    }
    /// Reports a failed request. Breaks (the connection ends) if the client
    /// never authenticated; otherwise the session ignores requests until
    /// RESET.
    fn fail(&mut self, stream: &mut TcpStream, error: RequestError) -> io::Result<ControlFlow<()>> {
        let failure = match error {
            RequestError::Transport(error) => return Err(error),
            RequestError::Client(failure) => failure,
        };
        eprintln!(
            "event=bolt_request_failed connection_id={} code={}",
            self.id, failure.code
        );
        self.output = None;
        self.send(
            stream,
            &failure.value(),
            Instant::now() + self.config.write_timeout,
        )?;
        if !self.authenticated {
            return Ok(ControlFlow::Break(()));
        }
        self.state = State::Failed;
        Ok(ControlFlow::Continue(()))
    }
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
    /// Runs one request and sends its response. Each message is accepted
    /// only in the states the guards name; anything else is invalid.
    fn handle(
        &mut self,
        stream: &mut TcpStream,
        msg: Message,
        stop: &AtomicBool,
    ) -> Result<(), RequestError> {
        let response = match msg {
            Message::Hello(extra) if self.state == State::Connected => self.hello(extra)?,
            Message::Logon(extra) if self.minor >= 1 && self.state == State::Authentication => {
                self.logon(extra)?
            }
            Message::Logoff if self.minor >= 1 && self.state == State::Ready => self.logoff(),
            Message::Reset if self.authenticated => self.reset(),
            Message::Route { db, impersonate } if self.state == State::Ready => {
                self.route(db.as_deref(), impersonate)?
            }
            Message::Telemetry if self.minor >= 4 && self.state == State::Ready => success([]),
            Message::Unsupported if self.authenticated => {
                return Err(Failure::new(INVALID, "not supported yet").into());
            }
            Message::Run {
                text,
                params,
                extra,
            } if self.state == State::Ready => self.run(&text, &params, &extra, stop)?,
            Message::Pull { n, qid, discard } if self.state == State::Streaming => {
                self.pull(stream, n, qid, discard)?
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
    /// HELLO: Bolt 5.0 authenticates here; from 5.1 on LOGON does.
    fn hello(&mut self, extra: Map) -> Result<Value, Failure> {
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
        Ok(success([
            ("server", Value::String("Neo4j/5.0.0".into())),
            (
                "kala_version",
                Value::String(env!("CARGO_PKG_VERSION").into()),
            ),
            ("connection_id", Value::String(format!("kala-{}", self.id))),
            ("hints", Value::Map(hints)),
        ]))
    }
    /// LOGON (Bolt 5.1+).
    fn logon(&mut self, extra: Map) -> Result<Value, Failure> {
        self.authenticate(extra)?;
        self.authenticated = true;
        self.state = State::Ready;
        Ok(success([]))
    }
    /// LOGOFF (Bolt 5.1+): back to waiting for a LOGON.
    fn logoff(&mut self) -> Value {
        self.authenticated = false;
        self.identity = None;
        self.state = State::Authentication;
        success([])
    }
    /// RESET: drops any unsent result and clears a failure.
    fn reset(&mut self) -> Value {
        self.output = None;
        self.state = State::Ready;
        success([])
    }
    /// Shared authorization and existence checks; ROUTE reveals no more than
    /// RUN (its one exception, `system`, is explained in `route`).
    fn database(&self, requested: Option<&str>) -> Result<String, Failure> {
        let name = self.registry.name(requested)?;
        if !self
            .identity
            .as_ref()
            .is_some_and(|id| id.permits_read(name))
        {
            return Err(Failure::new(
                crate::error::FORBIDDEN,
                format!("access to database {name:?} is forbidden"),
            ));
        }
        self.registry.require(name)?;
        Ok(name.to_owned())
    }
    fn route(&self, requested: Option<&str>, impersonate: bool) -> Result<Value, Failure> {
        if impersonate {
            return Err(Failure::impersonation());
        }
        // Tools list databases on `system` through a routing URI, and RUN
        // lets any identity do that there, so the table for `system` is not
        // gated on reading it. It holds only the advertised address, which
        // the caller already gets for its own databases; `system` is a
        // reserved name, so nothing is learned from its existence; and RUN
        // still checks read access. Every other name answers exactly as RUN.
        let db = match self.registry.name(requested)? {
            "system" => "system".to_owned(),
            _ => self.database(requested)?,
        };
        let servers = ["WRITE", "READ", "ROUTE"]
            .into_iter()
            .map(|role| {
                Value::Map(
                    [
                        ("role".into(), Value::String(role.into())),
                        (
                            "addresses".into(),
                            Value::List(vec![Value::String(self.advertised_address.clone())]),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                )
            })
            .collect();
        eprintln!("event=bolt_route connection_id={} db={db}", self.id);
        Ok(success([(
            "rt",
            Value::Map(
                [
                    ("ttl".into(), Value::Integer(300)),
                    ("db".into(), Value::String(db)),
                    ("servers".into(), Value::List(servers)),
                ]
                .into_iter()
                .collect(),
            ),
        )]))
    }
    /// RUN: checks access to the database, runs the whole query and keeps
    /// its encoded rows for PULL / DISCARD.
    fn run(
        &mut self,
        text: &str,
        params: &Map,
        extra: &Map,
        stop: &AtomicBool,
    ) -> Result<Value, Failure> {
        // Before anything else: a query "as alice" must never run with the
        // connection's own grants.
        if crate::message::impersonated(extra)? {
            return Err(Failure::impersonation());
        }
        let requested = match extra.get("db") {
            None => None,
            Some(Value::String(s)) => Some(s.as_str()),
            _ => return Err(Failure::invalid()),
        };
        let name = self.registry.name(requested)?.to_owned();
        let system = name == "system";
        // Existence and authorization come before the query text is parsed,
        // so a caller learns nothing about its text (not even that it is
        // malformed) on a database it may not read.
        let statement = match self.database(Some(&name)) {
            Ok(_) => query::server_statement(text, system)?,
            // The one exemption: a pure database listing on `system` (where
            // tools ask for it) reads only registry names and the caller's
            // own grants, never the system world. Whatever else the text is,
            // malformed included, gets the database failure.
            Err(denied) if system => match query::server_statement(text, true) {
                Ok(Some(statement))
                    if matches!(statement.request, kala_lang::ServerRequest::Databases(_)) =>
                {
                    Some(statement)
                }
                _ => return Err(denied),
            },
            Err(denied) => return Err(denied),
        };
        self.started = Instant::now();
        eprintln!(
            "event=bolt_query_started connection_id={} db={name}",
            self.id
        );
        let output = if let Some(statement) = statement {
            let identity = self.identity.as_ref().ok_or_else(Failure::invalid)?;
            let rows = crate::metadata::rows(
                &statement.request,
                self.registry,
                identity,
                &self.advertised_address,
                stop,
            )?;
            query::run_server(
                &statement,
                rows,
                params,
                stop,
                self.config.max_message_bytes,
                self.config.max_depth,
                system,
            )?
        } else {
            let (_, world) = self.registry.get(Some(&name))?;
            query::run(
                &world,
                text,
                params,
                stop,
                self.config.max_message_bytes,
                self.config.max_depth,
            )?
        };
        let fields = Value::List(output.fields.iter().cloned().map(Value::String).collect());
        eprintln!(
            "event=bolt_query_completed connection_id={} rows={}",
            self.id,
            output.rows.len()
        );
        self.db = name;
        self.output = Some(output);
        self.state = State::Streaming;
        Ok(success([
            ("fields", fields),
            ("t_first", Value::Integer(elapsed(self.started))),
            ("qid", Value::Integer(0)),
        ]))
    }
    /// PULL, or DISCARD (`discard`): sends or drops the next `n` rows (-1 =
    /// all of them) and says whether more remain.
    fn pull(
        &mut self,
        stream: &mut TcpStream,
        n: i64,
        qid: i64,
        discard: bool,
    ) -> Result<Value, RequestError> {
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
                chunk::write_message(stream, &bytes, deadline).map_err(RequestError::Transport)?;
            }
        }
        if output.rows.is_empty() {
            self.output = None;
            self.state = State::Ready;
            Ok(success([
                ("t_last", Value::Integer(elapsed(self.started))),
                ("type", Value::String("r".into())),
                ("db", Value::String(self.db.clone())),
            ]))
        } else {
            Ok(success([("has_more", Value::Bool(true))]))
        }
    }
}
fn elapsed(start: Instant) -> i64 {
    i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX)
}
