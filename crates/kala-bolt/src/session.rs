//! One connection's Bolt state. Authentication failures are terminal.
use crate::transport::Peer;
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
    transport::Transport,
};
use crate::{
    transaction::{Options, Snapshot, Transaction},
    write,
};
use kala_lang::write::StagedWrite;
use std::io;
use std::ops::ControlFlow;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Connected,
    Authentication,
    Ready,
    Streaming,
    TxReady,
    TxStreaming,
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
    peer: Peer,
    advertised_address: String,
    output: Option<query::Output>,
    db: String,
    started: Instant,
    transaction: Option<Transaction>,
    summary: Option<Map>,
    /// The summary type of the last RUN, for a PULL that has no commit
    /// summary of its own (`r`; inside a transaction a write is `w`, `s`
    /// when it only edited the schema, `rw` once the transaction queried).
    run_type: &'static str,
    /// The open transaction expired while the next request was awaited.
    expired: bool,
    /// The accounts file as it was when `identity`'s grants were read.
    grants_seen: Option<crate::registry::Stamp>,
}
/// What every connection of one server shares.
pub(crate) struct Shared<'a> {
    pub registry: &'a Registry,
    pub auth: &'a dyn Authenticator,
    pub config: &'a ServerConfig,
    /// Set at shutdown; cancels running queries.
    pub stop: &'a AtomicBool,
}
/// Where a connection comes from, as the accepted (unwrapped) connection
/// reported it.
pub(crate) struct Origin {
    /// The peer identity used for account matching.
    pub peer: Peer,
    /// The reached endpoint, advertised when no address is configured.
    pub reached: crate::transport::ListenAddr,
}
pub(crate) fn serve(
    mut stream: Box<dyn Transport>,
    origin: Origin,
    id: u64,
    shared: &Shared<'_>,
) -> io::Result<()> {
    let config = shared.config;
    let Origin { peer, reached } = origin;
    let Some(minor) = handshake::negotiate(
        stream.as_mut(),
        config.handshake_timeout,
        config.write_timeout,
    )?
    else {
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
            .unwrap_or_else(|| reached.to_string()),
        output: None,
        db: String::new(),
        started: Instant::now(),
        transaction: None,
        summary: None,
        run_type: "r",
        expired: false,
        grants_seen: None,
    };
    loop {
        let Some(message) = session.receive(stream.as_mut())? else {
            return Ok(());
        };
        let deadline = Instant::now() + config.write_timeout;
        if matches!(message, Ok(Message::Goodbye)) {
            return Ok(());
        }
        // The transaction is already gone (rolled back, its slot free). RESET
        // must succeed whatever happened before, and GOODBYE (above) closes;
        // any other request is told why it cannot run.
        let message =
            if std::mem::take(&mut session.expired) && !matches!(message, Ok(Message::Reset)) {
                Err(write::timeout())
            } else {
                message
            };
        if session.state == State::Failed && !matches!(message, Ok(Message::Reset)) {
            session.send(stream.as_mut(), &Value::Structure(0x7e, vec![]), deadline)?;
            continue;
        }
        let result = match message {
            Ok(msg) => session.handle(stream.as_mut(), msg, shared.stop),
            Err(e) => Err(RequestError::Client(e)),
        };
        if let Err(error) = result
            && session.fail(stream.as_mut(), error)?.is_break()
        {
            return Ok(());
        }
        // The idle clock runs only while the server waits for the client:
        // it starts when the response has been sent, never while a request
        // is being worked on.
        if let Some(tx) = &mut session.transaction {
            tx.idle_deadline = Instant::now() + config.tx_idle_timeout;
        }
    }
}
impl Session<'_> {
    /// Reads and parses the next request. `None` ends the connection: the
    /// message was over the size limit or not PackStream, and the client
    /// has been told so (if it is still listening).
    fn receive(
        &mut self,
        stream: &mut dyn Transport,
    ) -> io::Result<Option<Result<Message, Failure>>> {
        let config = self.config;
        let expires = self.transaction.as_ref().map(Transaction::idle_expires);
        let mut expired = false;
        let bytes = match chunk::read_message_expiring(
            stream,
            config.max_message_bytes,
            config.idle_timeout,
            expires,
            || {
                self.transaction = None;
                self.output = None;
                expired = true;
            },
        ) {
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
        self.expired = expired;
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
    fn fail(
        &mut self,
        stream: &mut dyn Transport,
        error: RequestError,
    ) -> io::Result<ControlFlow<()>> {
        let failure = match error {
            RequestError::Transport(error) => return Err(error),
            RequestError::Client(failure) => failure,
        };
        eprintln!(
            "event=bolt_request_failed connection_id={} code={}",
            self.id, failure.code
        );
        self.output = None;
        self.transaction = None;
        self.summary = None;
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
    fn send(&self, stream: &mut dyn Transport, value: &Value, deadline: Instant) -> io::Result<()> {
        let deadline = self
            .transaction
            .as_ref()
            .map_or(deadline, |tx| tx.deadline.min(deadline));
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
        // Before the accounts are read: a change that lands in between is
        // then noticed (a newer stamp), never missed.
        self.grants_seen = self.registry.accounts_stamp().and_then(Result::ok);
        let identity = self
            .auth
            .authenticate_peer(&token, self.peer)
            .map_err(|_| {
                Failure::new(
                    "Neo.ClientError.Security.Unauthorized",
                    "authentication failed",
                )
            })?;
        // A Unix peer has no IP; `-` rather than a host name that could be
        // mistaken for a loopback TCP client.
        let (client_ip, transport) = match self.peer {
            Peer::Ip(ip) => (ip.to_string(), "tcp"),
            Peer::Local => ("-".to_owned(), "unix"),
        };
        eprintln!(
            "event=bolt_authenticated connection_id={} user={:?} client_ip={client_ip} peer={transport}",
            self.id,
            crate::error::escape(&identity.principal),
        );
        self.identity = Some(identity);
        Ok(())
    }
    /// Runs one request and sends its response. Each message is accepted
    /// only in the states the guards name; anything else is invalid.
    fn handle(
        &mut self,
        stream: &mut dyn Transport,
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
            Message::Begin(extra) if self.state == State::Ready => self.begin(&extra)?,
            Message::Commit if self.state == State::TxReady => self.end(true)?,
            Message::Rollback if self.state == State::TxReady => self.end(false)?,
            Message::Run {
                text,
                params,
                extra,
            } if matches!(self.state, State::Ready | State::TxReady) => {
                self.run(&text, &params, &extra, stop)?
            }
            Message::Pull { n, qid, discard }
                if matches!(self.state, State::Streaming | State::TxStreaming) =>
            {
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
        self.grants_seen = None;
        self.state = State::Authentication;
        success([])
    }
    /// RESET: drops any unsent result and clears a failure.
    fn reset(&mut self) -> Value {
        self.transaction = None;
        self.summary = None;
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
        if self.peer == Peer::Local && self.config.advertised_address.is_none() {
            return Err(Failure::new(
                INVALID,
                "routing over a Unix socket needs --advertised-address",
            ));
        }
        // Tools list databases on `system` through a routing URI, and RUN
        // lets any identity do that there, so the table for `system` is not
        // gated on reading it. It holds only the advertised address, which
        // the caller already gets for its own databases; `system` is a
        // reserved name, so nothing is learned from its existence; and RUN
        // still checks read access. Every other name answers exactly as RUN.
        let db = match self.registry.name(requested)? {
            "system" => "system".to_owned(),
            name => {
                if !self
                    .identity
                    .as_ref()
                    .is_some_and(|id| id.permits_read(name) || id.permits_write(name))
                {
                    return Err(Failure::new(
                        crate::error::FORBIDDEN,
                        format!("access to database {name:?} is forbidden"),
                    ));
                }
                self.registry.require(name)?;
                name.to_owned()
            }
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
    /// Re-reads this connection's grants if any account changed since they
    /// were read, so a privilege revoked (or an account dropped) with
    /// `kala user` stops a pooled connection's next transaction or write
    /// instead of lasting until its next login. Called at BEGIN and before
    /// an auto-commit write, not for reads: it costs one look at the
    /// accounts file, and a reload of the accounts only when it changed.
    fn refresh_grants(&mut self) -> Result<(), Failure> {
        let Some(stamp) = self.registry.accounts_stamp() else {
            return Ok(());
        };
        let stamp = stamp?;
        if self.grants_seen.as_ref() == Some(&stamp) {
            return Ok(());
        }
        let identity = self.identity.as_mut().ok_or_else(Failure::invalid)?;
        let current = self.auth.refresh(identity).map_err(|_| {
            Failure::new(
                crate::error::FORBIDDEN,
                "this connection's privileges could not be verified",
            )
        })?;
        self.grants_seen = Some(stamp);
        match current {
            Some(current) => {
                *identity = current;
                Ok(())
            }
            None => {
                // Fail closed for everything that follows on this connection.
                identity.grants.clear();
                Err(Failure::new(
                    crate::error::FORBIDDEN,
                    "this connection's account no longer exists",
                ))
            }
        }
    }
    fn write_allowed(&self, db: &str, read_mode: bool) -> Result<(), Failure> {
        let refused = if self.config.read_only {
            Some((
                crate::error::FORBIDDEN,
                "this server is read-only",
                "read_only",
            ))
        } else if db == "system" {
            Some((
                crate::error::FORBIDDEN,
                "the system database is written only with kala user",
                "system",
            ))
        } else if read_mode {
            Some((
                write::ACCESS,
                "a read transaction cannot write",
                "read_only",
            ))
        } else if !self
            .identity
            .as_ref()
            .is_some_and(|id| id.permits_write(db))
        {
            Some((
                crate::error::FORBIDDEN,
                "write privilege is required",
                "privilege",
            ))
        } else {
            None
        };
        if let Some((code, message, reason)) = refused {
            self.log_refused(db, reason);
            Err(Failure::new(code, message))
        } else {
            self.registry.require(db)
        }
    }
    fn log_refused(&self, db: &str, reason: &str) {
        eprintln!(
            "event=bolt_write_refused connection_id={} db={:?} user={:?} reason={reason}",
            self.id,
            crate::error::escape(db),
            self.identity
                .as_ref()
                .map(|id| crate::error::escape(&id.principal))
        );
    }
    fn log_commit(&self, db: &str, result: &kala_core::CommitResult, forms: usize) {
        if result.committed {
            eprintln!(
                "event=bolt_write_committed connection_id={} db={} user={:?} tx={} forms={forms} changed={}",
                self.id,
                crate::error::escape(db),
                self.identity
                    .as_ref()
                    .map(|id| crate::error::escape(&id.principal)),
                result.tx.0,
                result.changed
            );
        }
    }
    fn requested<'a>(&self, extra: &'a Map) -> Result<Option<&'a str>, Failure> {
        if crate::message::impersonated(extra)? {
            return Err(Failure::impersonation());
        }
        match extra.get("db") {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s)),
            _ => Err(Failure::invalid()),
        }
    }
    /// BEGIN, in any mode: pins the shared read snapshot (after any
    /// bookmark wait) and nothing else. Whether the transaction may write
    /// is decided when a write form arrives (`write_allowed`), and the
    /// writable handle and its slot are taken then (`Transaction::writer`):
    /// drivers begin in write mode by default, also for reads, on `system`
    /// and against a read-only server.
    fn begin(&mut self, extra: &Map) -> Result<Value, Failure> {
        let name = self.registry.name(self.requested(extra)?)?.to_owned();
        let options = Options::parse(extra, &name, self.config)?;
        self.refresh_grants()?;
        let identity = self.identity.as_ref().ok_or_else(Failure::invalid)?;
        if !identity.permits_read(&name) && !identity.permits_write(&name) {
            return Err(Failure::new(
                crate::error::FORBIDDEN,
                "read or write privilege is required",
            ));
        }
        self.registry.require(&name)?;
        let now = Instant::now();
        let snapshot = Snapshot::Read(self.registry.at_least(
            &name,
            options.bookmark,
            self.config.busy_timeout.min(options.timeout),
        )?);
        self.transaction = Some(Transaction {
            snapshot,
            db: name,
            read_only: options.read_only,
            concurrency: options.concurrency,
            forms: Vec::new(),
            bytes: 0,
            queried: false,
            summary_reads: false,
            mode_set: false,
            cypher_forms: Vec::new(),
            server_statement: false,
            pending: crate::reads::Reads::default(),
            begun: None,
            deadline: now + options.timeout,
            idle_deadline: Instant::now() + self.config.tx_idle_timeout,
        });
        if self
            .transaction
            .as_ref()
            .is_some_and(|t| Instant::now() >= t.deadline)
        {
            self.transaction = None;
            return Err(write::timeout());
        }
        self.state = State::TxReady;
        Ok(success([]))
    }
    fn end(&mut self, commit: bool) -> Result<Value, Failure> {
        let transaction = self.transaction.take().ok_or_else(Failure::invalid)?;
        self.state = State::Ready;
        if !commit {
            return Ok(success([]));
        }
        let db = transaction.db;
        let read = self
            .identity
            .as_ref()
            .is_some_and(|id| id.permits_read(&db));
        let forms = transaction.forms;
        let deadline = transaction.deadline;
        if let Snapshot::Write(mut tx, _slot) = transaction.snapshot {
            if forms.is_empty() {
                let txid = tx.snapshot_tx().0;
                return Ok(success([
                    ("db", Value::String(db.clone())),
                    ("type", Value::String("r".into())),
                    ("bookmark", Value::String(format!("kala:{db}:{txid}"))),
                ]));
            }
            tx.with(|t| t.set_deadline(deadline));
            let (world, result) = tx.commit();
            let result = result.map_err(|e| {
                self.log_refused(&db, "conflict");
                write::commit_error(&world, e, &forms, read)
            })?;
            self.log_commit(&db, &result, forms.len());
            let mut summary = write::Summary::new(self.config.max_message_bytes);
            let completed: Vec<_> = forms
                .into_iter()
                .map(|form| {
                    let report = form.clone().complete(&result);
                    StagedWrite {
                        ops: report.ops,
                        ..form
                    }
                })
                .collect();
            summary.add(&world, &result, &completed, read);
            for index in transaction.cypher_forms {
                summary.operations(index + 1, &completed[index].ops);
            }
            Ok(Value::Structure(
                0x70,
                vec![Value::Map(summary.metadata(
                    &db,
                    transaction.summary_reads,
                    self.config.max_message_bytes,
                    self.config.max_depth,
                ))],
            ))
        } else {
            let Snapshot::Read(world) = transaction.snapshot else {
                unreachable!()
            };
            Ok(success([
                ("db", Value::String(db.clone())),
                ("type", Value::String("r".into())),
                (
                    "bookmark",
                    Value::String(format!("kala:{db}:{}", world.latest_tx().0)),
                ),
            ]))
        }
    }
    /// Classify once through kala-lang, after database authorization.
    fn run(
        &mut self,
        text: &str,
        params: &Map,
        extra: &Map,
        stop: &AtomicBool,
    ) -> Result<Value, Failure> {
        let requested = self.requested(extra)?;
        let name = if let Some(tx) = &self.transaction {
            if requested.is_some_and(|name| name != tx.db) {
                return Err(Failure::new(
                    INVALID,
                    "RUN cannot change a transaction's database",
                ));
            }
            tx.db.clone()
        } else {
            self.registry.name(requested)?.to_owned()
        };
        let options = Options::parse(extra, &name, self.config)?;
        let identity = self.identity.as_ref().ok_or_else(Failure::invalid)?;
        let read = identity.permits_read(&name);
        let write_grant = identity.permits_write(&name);
        let system = name == "system";
        let source = crate::language::detect(text);
        let statement = if !read && !write_grant {
            // Preserve the existing SHOW DATABASES exception and refusal-before-parse behavior.
            let denied = self.database(Some(&name)).err().unwrap_or_else(|| {
                Failure::new(crate::error::FORBIDDEN, "read privilege is required")
            });
            if !system {
                return Err(denied);
            }
            match query::server_statement(text, true) {
                Ok(Some(s)) if matches!(s.request, kala_lang::ServerRequest::Databases(_)) => {
                    kala_lang::Statement::Server(Box::new(s))
                }
                _ => return Err(denied),
            }
        } else {
            self.registry.require(&name)?;
            let source = source
                .as_ref()
                .map_err(|_| Failure::new(crate::error::SYNTAX, "Gremlin is not supported yet"))?;
            kala_lang::classify(source.front_end, &source.text, &|attr| {
                system && attr == "password_hash"
            })?
        };
        self.started = Instant::now();
        self.summary = None;
        self.run_type = "r";
        let output = match statement {
            kala_lang::Statement::Concurrency(mode) => {
                self.allow_write_run(&name, &options)?;
                let tx = self.transaction.as_mut().ok_or_else(|| Failure::new(INVALID,
                    "use BEGIN metadata or put SET CONCURRENCY_MODE first in an explicit transaction"))?;
                if tx.mode_set || tx.queried || tx.server_statement || !tx.forms.is_empty() {
                    return Err(Failure::new(
                        INVALID,
                        "concurrency is allowed only in the first statement of a transaction",
                    ));
                }
                tx.concurrency = mode;
                tx.mode_set = true;
                query::Output::empty()
            }
            kala_lang::Statement::CypherWrite(statement) => {
                let held = self.allow_write_run(&name, &options)?;
                self.cypher_run(&name, &statement, params, text.len(), &options, held, stop)
                    .inspect_err(|failure| self.log_refused(&name, refusal_reason(failure)))?
            }
            kala_lang::Statement::Writes => {
                let read = self.allow_write_run(&name, &options)?.read;
                if !params.is_empty() {
                    return Err(Failure::new(
                        INVALID,
                        "Datalog write forms do not accept parameters",
                    ));
                }
                let source = source?;
                self.write_run(&name, &source.text, &options, read)
                    .inspect_err(|failure| self.log_refused(&name, refusal_reason(failure)))?
            }
            kala_lang::Statement::Server(statement) => {
                if self
                    .transaction
                    .as_ref()
                    .is_some_and(|tx| !tx.forms.is_empty())
                {
                    return Err(Failure::new(
                        INVALID,
                        "server statements cannot run inside a write transaction",
                    ));
                }
                if !read
                    && !(system
                        && matches!(statement.request, kala_lang::ServerRequest::Databases(_)))
                {
                    return Err(Failure::new(
                        crate::error::FORBIDDEN,
                        "read privilege is required",
                    ));
                }
                let identity = self.identity.as_ref().ok_or_else(Failure::invalid)?;
                let deadline = self
                    .transaction
                    .as_ref()
                    .map_or(self.started + options.timeout, |tx| tx.deadline);
                if let Some(tx) = &mut self.transaction {
                    tx.server_statement = true;
                }
                query::until(deadline, |deadline| {
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
                        query::Stop {
                            flag: stop,
                            deadline,
                        },
                        self.config.max_message_bytes,
                        self.config.max_depth,
                        system,
                    )
                })?
            }
            kala_lang::Statement::Query => {
                if !read {
                    return Err(Failure::new(
                        crate::error::FORBIDDEN,
                        "read privilege is required",
                    ));
                }
                eprintln!(
                    "event=bolt_query_started connection_id={} db={name}",
                    self.id
                );
                let output = if let Some(tx) = &mut self.transaction {
                    let output = query::run(
                        tx.world(),
                        text,
                        params,
                        stop,
                        self.config.max_message_bytes,
                        self.config.max_depth,
                        tx.deadline,
                    )?;
                    tx.record(&output.reads, true);
                    output
                } else {
                    let world = self.registry.at_least(
                        &name,
                        options.bookmark,
                        self.config.busy_timeout.min(options.timeout),
                    )?;
                    query::run(
                        &world,
                        text,
                        params,
                        stop,
                        self.config.max_message_bytes,
                        self.config.max_depth,
                        self.started + options.timeout,
                    )?
                };
                eprintln!(
                    "event=bolt_query_completed connection_id={} rows={}",
                    self.id,
                    output.rows.len()
                );
                output
            }
        };
        if self
            .transaction
            .as_ref()
            .is_some_and(|t| Instant::now() >= t.deadline)
        {
            self.transaction = None;
            return Err(write::timeout());
        }
        let fields = Value::List(output.fields.iter().cloned().map(Value::String).collect());
        self.db = name;
        self.output = Some(output);
        self.state = if self.transaction.is_some() {
            State::TxStreaming
        } else {
            State::Streaming
        };
        Ok(success([
            ("fields", fields),
            ("t_first", Value::Integer(elapsed(self.started))),
            ("qid", Value::Integer(0)),
        ]))
    }
    fn allow_write_run(
        &mut self,
        db: &str,
        options: &Options,
    ) -> Result<write::Privileges, Failure> {
        if self.transaction.is_none() {
            self.refresh_grants()?;
        }
        let mode = self
            .transaction
            .as_ref()
            .map_or(options.read_only, |tx| tx.read_only);
        self.write_allowed(db, mode)?;
        if self.config.max_message_bytes < 1024 || self.config.max_depth < 8 {
            return Err(Failure::new(
                INVALID,
                "writes require a message limit of at least 1024 bytes and depth 8",
            ));
        }
        if self
            .transaction
            .as_ref()
            .is_some_and(|tx| tx.server_statement)
        {
            return Err(Failure::new(
                INVALID,
                "server statements and writes cannot share a transaction",
            ));
        }
        let identity = self.identity.as_ref().ok_or_else(Failure::invalid)?;
        Ok(write::Privileges {
            read: identity.permits_read(db),
            admin: identity.permits_admin(db),
        })
    }
    #[allow(clippy::too_many_arguments)]
    fn cypher_run(
        &mut self,
        db: &str,
        statement: &kala_lang::write::WriteStatement,
        given: &Map,
        text_len: usize,
        options: &Options,
        held: write::Privileges,
        flag: &AtomicBool,
    ) -> Result<query::Output, Failure> {
        use kala_lang::write::{compile_statement, run_statement, stage_statement};
        let fields = statement.fields();
        let rw = statement.has_reading_part() || !fields.is_empty();
        let prior = self.transaction.as_ref().map_or(0, |tx| tx.forms.len());
        write::admit_needs(statement.requires_read(), false, false, prior, held)?;
        let deadline = self
            .transaction
            .as_ref()
            .map_or(self.started + options.timeout, |tx| tx.deadline);
        let stop = query::Stop { flag, deadline };
        let max = self.config.max_message_bytes;
        let depth = self.config.max_depth;
        // RUN publishes its field names before PULL. Verify that response
        // fits before we append anything, even for a long quoted alias.
        let header = success([
            (
                "fields",
                Value::List(fields.iter().cloned().map(Value::String).collect()),
            ),
            ("t_first", Value::Integer(i64::MAX)),
            ("qid", Value::Integer(0)),
        ]);
        crate::packstream::encode_with_limits(&header, max, depth)
            .map_err(|_| Failure::new(INVALID, "RETURN column names exceed the message limit"))?;
        // Parameters take the kinds the reading part's compilation computed,
        // as for a read query: a handle given for a ref position resolves to
        // its entity, and a wrongly typed value is refused before evaluation.
        let prepare = |world: &kala_core::World, limits: kala_query::Limits| {
            let compiled = compile_statement(world, statement)
                .map_err(|e| write::statement_error(e, held.read))?;
            let params = query::parameters(world, compiled.parameter_kinds(), given, true)?;
            let run = kala_query::RunOptions::new(limits)
                .with_cancel(flag)
                .with_deadline(deadline);
            let input = query::until(deadline, |_| {
                run_statement(world, statement, compiled, &params, run)
                    .map_err(|e| write::statement_error(e, held.read))
            })?;
            Ok::<_, Failure>((params, input))
        };
        let full = kala_query::Limits::default();
        if let Some(tx) = &mut self.transaction {
            let account = self
                .identity
                .as_ref()
                .map(|id| format!("{}@{}", id.principal, id.host))
                .ok_or_else(Failure::invalid)?;
            let param_size =
                crate::packstream::encode_with_limits(&Value::Map(given.clone()), max, depth)
                    .map_err(|_| Failure::new(INVALID, "parameter limit exceeded"))?
                    .len();
            write::charge(&mut tx.bytes, text_len.saturating_add(param_size))?;
            let (params, input) = prepare(tx.world(), full)?;
            // Pin BEGIN even for the one empty input row of a blind CREATE.
            // A later MATCH reads that same snapshot.
            tx.record(input.reads(), rw);
            let owned = tx.writer(self.registry, self.config, &account, held.admin)?;
            let (output, staged, retained_bytes) = owned.with(|t| {
                let result = stage_statement(t, statement, input, &params)
                    .map_err(|e| write::statement_error(e, held.read))?;
                let output = query::render(
                    query::RenderWorld::Staged(t),
                    fields,
                    result.rows,
                    result.tick,
                    Default::default(),
                    query::RenderOptions::write(stop, max, depth),
                )?;
                Ok::<_, Failure>((output, result.staged, result.retained_bytes))
            })?;
            write::charge(&mut tx.bytes, retained_bytes)?;
            self.run_type = if rw {
                "rw"
            } else if statement.schema_only() {
                "s"
            } else {
                "w"
            };
            tx.cypher_forms.push(tx.forms.len());
            tx.forms.push(staged);
            Ok(output)
        } else {
            // Where the reading part runs. A write never replays the log to
            // get a snapshot (`Registry::cached_at_least` only ever applies
            // the log's tail to the cached one):
            //
            // - A blind statement (nothing but CREATE or a constraint, no
            //   RETURN) has no reading part to speak of, one empty row: it
            //   takes no snapshot and is compiled on the writer.
            // - A statement that reads, in the default mode, reads the
            //   cached reader snapshot BEFORE the database's writer is
            //   taken, when that snapshot is the current committed state:
            //   however heavy the reading part is, no other writer waits
            //   for it. A commit that lands between this read and the stage
            //   below is checked precisely: staging records the read keys
            //   as read at the snapshot's tx (`Txn::set_read_version`), so
            //   only a commit that touched what was read is a conflict
            //   (transient: drivers retry it).
            // - Otherwise (no current snapshot to be had without a replay,
            //   or weak mode, which checks no reads and so must not read a
            //   state a commit may have overtaken) the reading part runs on
            //   the writer, under the mutex, within `UNDER_WRITER_STEPS`.
            let reads = statement.requires_read();
            let snapshot = if reads && options.concurrency != kala_core::Concurrency::Weak {
                self.registry.cached_at_least(db, options.bookmark)?
            } else {
                None
            };
            let early = match snapshot {
                Some(snapshot) => {
                    let (params, input) = prepare(&snapshot, full)?;
                    // A statement that read lands its tick-less writes on
                    // the latest tick its reading part saw.
                    let seen = (snapshot.latest_tx().0, snapshot.latest_tick());
                    drop(snapshot);
                    #[cfg(feature = "test-hooks")]
                    if let Some(hook) = &self.config.after_statement_read {
                        (hook.0)();
                    }
                    Some((params, input, seen))
                }
                None => None,
            };
            let pinned = early.as_ref().map_or(0, |(_, _, (tx, _))| *tx);
            let (output, summary) = self.registry.write(
                db,
                self.config
                    .busy_timeout
                    .min(deadline.saturating_duration_since(Instant::now())),
                options.bookmark.max(pinned),
                |world| {
                    let mut tx = world.begin();
                    tx.set_concurrency(options.concurrency);
                    tx.set_schema_additions_only(!held.admin);
                    tx.set_deadline(deadline);
                    let (params, input) = match early {
                        Some((params, input, (_, tick))) => {
                            tx.set_start_tick(tick);
                            (params, input)
                        }
                        None => {
                            // Other writers wait while this runs, so a
                            // reading part gets a tenth of a query's steps;
                            // staging then has the whole statement's budget
                            // again, as on the other path.
                            let limits = kala_query::Limits {
                                max_steps: if reads { UNDER_WRITER_STEPS } else { full.max_steps },
                                ..full
                            };
                            let (params, input) = prepare(tx.base_world(), limits)?;
                            (params, input.with_stage_limits(full))
                        }
                    };
                    let mut staged = stage_statement(&mut tx, statement, input, &params)
                        .map_err(|e| write::statement_error(e, held.read))?;
                    // Render RETURN from the staged transaction before the
                    // durable append, under the limits the real rendering
                    // has: a result that cannot be sent refuses the
                    // statement while nothing is committed. Final IDs can
                    // only be a few digits wider (a commit renumbering over
                    // another process's writes), hence the margin.
                    if !fields.is_empty() {
                        query::render(
                            query::RenderWorld::Staged(&tx),
                            fields.clone(),
                            staged.rows.clone(),
                            staged.tick,
                            Default::default(),
                            query::RenderOptions::write(stop, max.saturating_sub(64), depth)
                                .preview(),
                        )?;
                    }
                    if Instant::now() >= deadline {
                        return Err(write::timeout());
                    }
                    let result = tx.commit().map_err(|e| {
                        write::commit_error(
                            world,
                            e,
                            std::slice::from_ref(&staged.staged),
                            held.read,
                        )
                    })?;
                    self.log_commit(db, &result, 1);
                    let outcome = if result.committed {
                        "write committed"
                    } else {
                        "write completed without changes"
                    };
                    // Past the append nothing is retryable: a failure the
                    // preview could not see is reported as committed.
                    let after_commit = || {
                        Failure::new(
                            crate::error::STORE,
                            format!(
                                "{outcome} at tx {} (bookmark kala:{db}:{}); RETURN could not be rendered",
                                result.tx.0, result.tx.0
                            ),
                        )
                    };
                    let rows = staged
                        .committed_rows(world, &result.ids)
                        .map_err(|_| after_commit())?;
                    let output = query::render(
                        query::RenderWorld::Snapshot(world, result.tx),
                        fields,
                        rows,
                        staged.tick,
                        Default::default(),
                        query::RenderOptions::write(stop, max, depth),
                    )
                    .map_err(|_| after_commit())?;
                    let ops = staged.staged.clone().complete(&result).ops;
                    let form = StagedWrite {
                        ops,
                        ..staged.staged
                    };
                    let mut summary = write::Summary::new(max);
                    summary.add(world, &result, std::slice::from_ref(&form), held.read);
                    summary.operations(1, &form.ops);
                    Ok((output, summary.metadata(db, rw, max, depth)))
                },
            )?;
            self.summary = Some(summary);
            Ok(output)
        }
    }
    fn write_run(
        &mut self,
        db: &str,
        text: &str,
        options: &Options,
        read: bool,
    ) -> Result<query::Output, Failure> {
        let forms = kala_lang::write::write_forms(text).map_err(write::parse_error)?;
        // Every form of the RUN is parsed and admitted (its privileges and
        // its place in the transaction) before a writable handle is opened
        // or anything is staged: a syntax error or a privilege refusal
        // anywhere in the text means this RUN staged and committed nothing.
        // The text is bounded by the message limit and the forms by the
        // parser's own limits.
        let forms = forms
            .collect::<Result<Vec<_>, _>>()
            .map_err(write::parse_error)?;
        // Write slots are counted per authenticated account (user and the
        // host selector that matched), as the identity names it.
        let account = self
            .identity
            .as_ref()
            .map(|id| format!("{}@{}", id.principal, id.host))
            .ok_or_else(Failure::invalid)?;
        let held = write::Privileges {
            read,
            admin: self
                .identity
                .as_ref()
                .is_some_and(|id| id.permits_admin(db)),
        };
        let mut output = write::output();
        let max_message = self.config.max_message_bytes;
        let max_depth = self.config.max_depth;
        if let Some(tx) = &mut self.transaction {
            tx.bytes = tx
                .bytes
                .checked_add(text.len())
                .filter(|n| *n <= write::MAX_STAGED_BYTES)
                .ok_or_else(|| {
                    Failure::new(INVALID, "transaction exceeds the staged-text byte limit")
                })?;
            for (index, form) in forms.iter().enumerate() {
                write::admit(
                    form,
                    tx.forms.len() + index + usize::from(tx.mode_set),
                    held,
                )?;
            }
            let mut data = false;
            for (index, form) in forms.iter().enumerate() {
                let expires = tx.deadline;
                let owned = tx.writer(self.registry, self.config, &account, held.admin)?;
                let staged = owned.with(|t| {
                    t.set_deadline(expires);
                    write::stage(t, form, read)
                })?;
                data |= staged.ops.iter().any(|op| write::writes_data(op.kind));
                // Label using the transaction so provisional node types resolve.
                owned.with(|t| {
                    write::rows(
                        &mut output,
                        &staged.ops,
                        index + 1,
                        read,
                        |id| t.format_id(id),
                        max_message,
                        max_depth,
                    )
                })?;
                tx.forms.push(staged);
            }
            self.run_type = if tx.summary_reads {
                "rw"
            } else if data {
                "w"
            } else {
                "s"
            };
        } else {
            // Each form is its own transaction, the first of it.
            for form in &forms {
                write::admit(form, 0, held)?;
            }
            let deadline = self.started + options.timeout;
            let mut summary = write::Summary::new(self.config.max_message_bytes);
            self.registry.write(
                db,
                self.config
                    .busy_timeout
                    .min(deadline.saturating_duration_since(Instant::now())),
                options.bookmark,
                |world| {
                    let mut committed = 0;
                    for (index, form) in forms.iter().enumerate() {
                        let prior = committed;
                        let result = (|| {
                            if Instant::now() >= deadline {
                                return Err(write::timeout());
                            }
                            let mut tx = world.begin();
                            tx.set_concurrency(options.concurrency);
                            tx.set_deadline(deadline);
                            tx.set_schema_additions_only(!held.admin);
                            let staged = write::stage(&mut tx, form, read)?;
                            // Check record size before durable append; final IDs have fixed encoding bounds.
                            let mut preview = write::output();
                            let mut preview_ops = staged.ops.clone();
                            for op in &mut preview_ops {
                                op.entries = vec![kala_core::EntryId(i64::MAX as u64); op.changed];
                            }
                            write::rows(
                                &mut preview,
                                &preview_ops,
                                index + 1,
                                read,
                                |id| tx.format_id(id),
                                max_message.saturating_sub(64),
                                max_depth,
                            )?;
                            if output.encoded_bytes.saturating_add(preview.encoded_bytes)
                                > write::MAX_STAGED_BYTES
                            {
                                return Err(Failure::new(
                                    INVALID,
                                    "write result exceeds the 16 MiB result limit",
                                ));
                            }
                            if Instant::now() >= deadline {
                                return Err(write::timeout());
                            }
                            let result = tx.commit().map_err(|e| {
                                write::commit_error(world, e, std::slice::from_ref(&staged), read)
                            })?;
                            committed += usize::from(result.committed);
                            self.log_commit(db, &result, 1);
                            let report = staged.clone().complete(&result);
                            summary.add(
                                world,
                                &result,
                                &[StagedWrite {
                                    ops: report.ops.clone(),
                                    ..staged
                                }],
                                read,
                            );
                            write::rows(
                                &mut output,
                                &report.ops,
                                index + 1,
                                read,
                                |id| world.format_id(id),
                                max_message,
                                max_depth,
                            )
                            .map_err(|_| {
                                Failure::new(
                                    crate::error::STORE,
                                    "write committed; result could not be encoded",
                                )
                            })?;
                            Ok(())
                        })();
                        if let Err(failure) = result {
                            return Err(write::form_failed(
                                failure,
                                index + 1,
                                prior,
                                committed,
                                db,
                                summary.tx,
                            ));
                        }
                    }
                    Ok(())
                },
            )?;
            self.summary = Some(summary.metadata(
                db,
                false,
                self.config.max_message_bytes,
                self.config.max_depth,
            ));
        }
        Ok(output)
    }
    /// PULL, or DISCARD (`discard`): sends or drops the next `n` rows (-1 =
    /// all of them) and says whether more remain.
    fn pull(
        &mut self,
        stream: &mut dyn Transport,
        n: i64,
        qid: i64,
        discard: bool,
    ) -> Result<Value, RequestError> {
        if qid != -1 && qid != 0 {
            return Err(
                Failure::new(INVALID, "only the last result (qid 0 or -1) is available").into(),
            );
        }
        let output = self.output.as_mut().ok_or_else(Failure::invalid)?;
        let count = if n == -1 {
            output.rows.len()
        } else {
            usize::try_from(n)
                .unwrap_or(usize::MAX)
                .min(output.rows.len())
        };
        let deadline = self
            .transaction
            .as_ref()
            .map_or(Instant::now() + self.config.write_timeout, |tx| {
                tx.deadline.min(Instant::now() + self.config.write_timeout)
            });
        for _ in 0..count {
            let exposes = output.exposes.pop_front().unwrap_or(false);
            if !discard
                && exposes
                && let Some(tx) = &mut self.transaction
            {
                tx.written()
                    .ok_or_else(Failure::invalid)?
                    .with(|t| t.set_ids_exposed(true));
            }
            if let Some(bytes) = output.rows.pop_front()
                && !discard
            {
                chunk::write_message(stream, &bytes, deadline).map_err(RequestError::Transport)?;
            }
        }
        if output.rows.is_empty() {
            self.output = None;
            self.state = if self.transaction.is_some() {
                State::TxReady
            } else {
                State::Ready
            };
            let mut summary = self.summary.take().unwrap_or_else(|| {
                [
                    ("type".into(), Value::String(self.run_type.into())),
                    ("db".into(), Value::String(self.db.clone())),
                ]
                .into()
            });
            summary.insert("t_last".into(), Value::Integer(elapsed(self.started)));
            Ok(Value::Structure(0x70, vec![Value::Map(summary)]))
        } else {
            Ok(success([("has_more", Value::Bool(true))]))
        }
    }
}
/// The `reason=` of a refused write's log line: never the statement's text
/// or values, only which kind of refusal it was.
/// The step budget of an auto-commit Cypher write's reading part when it
/// has to run on the writable handle, i.e. while every other writer of the
/// database waits: a tenth of a query's default (10,000,000). The rows it
/// may hand to the write clauses (10,000) and the statement's deadline bound
/// it as everywhere else.
const UNDER_WRITER_STEPS: u64 = 1_000_000;

fn refusal_reason(failure: &Failure) -> &'static str {
    if failure.code == crate::error::FORBIDDEN {
        "privilege"
    } else if failure.code.starts_with("Neo.TransientError") {
        "conflict"
    } else {
        "statement"
    }
}
fn elapsed(start: Instant) -> i64 {
    i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX)
}
