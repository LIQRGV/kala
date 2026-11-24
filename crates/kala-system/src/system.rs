//! The reserved, append-only system world. Only these helpers write account facts.
use crate::auth::{AccountName, Credentials, HostPattern, Identity, Plugin, hash_password};
use argon2::password_hash::rand_core::{OsRng, RngCore};
use kala_core::{AttrDef, Cardinality, EntityId, Kind, Schema, Tick, Txn, Value, ValueType, World};
use std::{collections::BTreeSet, fmt, fs, io, path::Path};

/// Content marker; filenames alone never identify a system world.
pub const SYSTEM_MARKER: &str = kala_core::SYSTEM_NODE_TYPE;
const VERSION_HANDLE: &str = "_kala_system";
const ATTRS: [&str; 5] = ["user", "host", "plugin", "password_hash", "grant"];

/// Account-management errors deliberately never contain input values or hashes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemError {
    /// User name or user@host shape is invalid.
    InvalidAccount,
    /// Host selector is not supported.
    InvalidHost,
    /// Privilege or database is invalid.
    InvalidGrant,
    /// A password must not be empty.
    EmptyPassword,
    /// Trust requires one of the explicit loopback selectors.
    TrustRequiresLoopback,
    /// An active account already has this identifier.
    AccountExists,
    /// No active account has this identifier.
    AccountNotFound,
    /// An operation would remove the last admin:* account.
    LastAdmin,
    /// An ordinary world occupies the reserved path.
    ReservedSystem,
    /// The system schema/version or account facts are inconsistent.
    InvalidSystem,
    /// Another account-management command holds the native writer lock.
    Busy,
    /// The system world could not be read or written.
    Store,
    /// Password hashing or OS randomness failed.
    Password,
    /// The server directory does not exist (it is never created).
    NoSuchDirectory,
    /// The system world's last transaction is damaged; writing would cut it
    /// to a quarantine file, which needs the caller's acknowledgement.
    RecoveryRequired {
        /// The size of the damaged tail.
        bytes: u64,
    },
    /// `system.kala` was written by an older Kala log format, which this
    /// build doesn't read (there is no dual-format reading). The file is
    /// left untouched.
    OldFormat {
        /// The file's log format version.
        version: u32,
    },
}
impl fmt::Display for SystemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::InvalidAccount => {
                "account must be USER@HOST with a nonempty ASCII user name of at most 64 bytes"
            }
            Self::InvalidHost => {
                "invalid host selector; use an IP, CIDR, trailing IPv4 wildcard, localhost, or %"
            }
            Self::InvalidGrant => {
                "invalid grant; use read, write, backup, or admin and a database name or *"
            }
            Self::EmptyPassword => "empty passwords are refused",
            Self::TrustRequiresLoopback => "trust requires localhost, 127.0.0.1, or ::1",
            Self::AccountExists => "account already exists",
            Self::AccountNotFound => "account was not found",
            Self::LastAdmin => "cannot remove the last account with admin:*",
            Self::ReservedSystem => "system is reserved for accounts; use kala user",
            Self::InvalidSystem => "invalid or unsupported system world",
            Self::Busy => "system world is being written by another writer; retry the command",
            Self::Store => "system world could not be read or written",
            Self::Password => "password hashing failed",
            Self::NoSuchDirectory => "no such directory",
            Self::RecoveryRequired { .. } => {
                "the last transaction of the system world is damaged; writing would cut it \
                 to a quarantine file, so the recovery must be acknowledged"
            }
            Self::OldFormat { version } => {
                return write!(
                    f,
                    "the accounts database (system.kala) was written by an older Kala log format \
                     (version {version}), which this build doesn't read. Dump the directory with \
                     the older kala binary that wrote it (kala dump --all DIR -o BACKUP), then load \
                     each dump into a new directory with this one (kala transact --create \
                     NEW_DIR/system.kala BACKUP/system.dump.edn, and the same for every world). \
                     The old files are left untouched"
                );
            }
        };
        f.write_str(text)
    }
}
impl std::error::Error for SystemError {}
fn store(error: kala_core::Error) -> SystemError {
    match error {
        kala_core::Error::WorldLocked => SystemError::Busy,
        // Another account change was committed after this one read the
        // accounts; nothing was written. `apply` runs the change again.
        kala_core::Error::Conflict(_) => SystemError::Busy,
        kala_core::Error::RecoveryRequired { bytes } => SystemError::RecoveryRequired { bytes },
        // Only the version number: nothing of the file's content.
        kala_core::Error::UnsupportedVersion(version)
            if version < kala_core::log::LOG_FORMAT_VERSION =>
        {
            SystemError::OldFormat { version }
        }
        _ => SystemError::Store,
    }
}
/// Public account fields. Password hashes are never part of this representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSummary {
    /// Case-sensitive user name.
    pub user: String,
    /// Canonical host selector.
    pub host: String,
    /// Authentication mechanism.
    pub plugin: Plugin,
    /// Database privileges in `privilege:database` form.
    pub grants: Vec<String>,
}
#[derive(Debug)]
pub(crate) struct Account {
    id: EntityId,
    pub(crate) summary: AccountSummary,
    pub(crate) host: HostPattern,
    pub(crate) password: Option<Credentials>,
}
impl Account {
    pub(crate) fn identity(&self) -> Identity {
        Identity {
            principal: self.summary.user.clone(),
            host: self.summary.host.clone(),
            grants: self.summary.grants.clone(),
        }
    }
}
fn schema() -> Schema {
    let mut schema = Schema::new()
        .node_type(SYSTEM_MARKER)
        .node_type("ACCOUNT")
        .attribute(AttrDef::new(
            "schema_version",
            ValueType::I64,
            Kind::State,
            Cardinality::One,
        ));
    for attr in ATTRS {
        let def = AttrDef::new(
            attr,
            ValueType::Str,
            Kind::State,
            if attr == "grant" {
                Cardinality::Many
            } else {
                Cardinality::One
            },
        );
        schema = schema.attribute(if attr == "password_hash" {
            def.unindexed()
        } else {
            def
        });
    }
    schema
}
/// Recognize the system marker by content, including renamed copies.
pub fn is_system(world: &World) -> bool {
    world.is_system()
}
/// The system attribute no query may read: comparisons, aggregates and
/// negations over it would reveal hashes even with output redacted.
const SECRET_ATTR: &str = "password_hash";
/// Whether a query on the system world may not name `attr`: the secret
/// attribute itself, or any name that resolves to it (an inverse name).
pub fn is_secret_attribute(world: &World, attr: &str) -> bool {
    attr == SECRET_ATTR
        || world
            .attributes()
            .any(|def| def.name == SECRET_ATTR && def.inverse.as_deref() == Some(attr))
}
fn validate(world: &World) -> Result<(), SystemError> {
    if !is_system(world) {
        return Err(SystemError::ReservedSystem);
    }
    let marker = world
        .entity_by_handle(VERSION_HANDLE)
        .ok_or(SystemError::InvalidSystem)?;
    if world
        .as_of(marker, "schema_version", world.latest_tick())
        .map_err(store)?
        != vec![Value::I64(1)]
    {
        return Err(SystemError::InvalidSystem);
    }
    for attr in ATTRS {
        let def = world.attribute(attr).ok_or(SystemError::InvalidSystem)?;
        if def.value_type != ValueType::Str
            || def.kind != Kind::State
            || def.cardinality
                != (if attr == "grant" {
                    Cardinality::Many
                } else {
                    Cardinality::One
                })
            || (attr == "password_hash" && def.indexed)
        {
            return Err(SystemError::InvalidSystem);
        }
    }
    Ok(())
}
fn existing(directory: &Path) -> Result<(), SystemError> {
    let world = World::open_read_only(&directory.join("system.kala")).map_err(store)?;
    validate(&world)?;
    read_accounts(&world)?;
    Ok(())
}
/// The server directory must already exist: account commands and the
/// server never create it (only `system.kala` inside it).
fn require_directory(directory: &Path) -> Result<(), SystemError> {
    match fs::metadata(directory) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(SystemError::NoSuchDirectory),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(SystemError::NoSuchDirectory),
        Err(_) => Err(SystemError::Store),
    }
}
/// Create the system world once, atomically publishing a complete root bootstrap.
/// Returns true only for the process that created it. The directory itself
/// must exist ([`SystemError::NoSuchDirectory`] otherwise).
pub fn ensure(directory: impl AsRef<Path>) -> Result<bool, SystemError> {
    let directory = directory.as_ref();
    require_directory(directory)?;
    let path = directory.join("system.kala");
    match fs::symlink_metadata(&path) {
        Ok(meta) => {
            if !meta.file_type().is_file() {
                return Err(SystemError::InvalidSystem);
            }
            existing(directory)?;
            return Ok(false);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err(SystemError::Store),
    }
    // A private staging directory protects genesis too; publishing the complete
    // file avoids ever exposing a schema without its root account after a crash.
    let mut random = [0u8; 16];
    OsRng
        .try_fill_bytes(&mut random)
        .map_err(|_| SystemError::Store)?;
    let name = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let staging = directory.join(format!(".kala-system-{name}"));
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&staging).map_err(|_| SystemError::Store)?;
    let temporary = staging.join("system.kala");
    let result = (|| {
        let mut world = World::create(&temporary, schema()).map_err(store)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
                .map_err(|_| SystemError::Store)?;
        }
        let mut txn = world.begin();
        txn.set_default_tick(Tick(0));
        let marker = txn
            .create_entity(SYSTEM_MARKER, Some(VERSION_HANDLE))
            .map_err(store)?;
        txn.assert(marker, "schema_version", Value::I64(1), None)
            .map_err(store)?;
        let root = txn
            .create_entity("ACCOUNT", Some("root@localhost"))
            .map_err(store)?;
        for (attr, value) in [
            ("user", "root"),
            ("host", "localhost"),
            ("plugin", "trust"),
            ("grant", "admin:*"),
        ] {
            txn.assert(root, attr, Value::Str(value.into()), None)
                .map_err(store)?;
        }
        txn.commit().map_err(store)?;
        // hard_link publishes without ever replacing an existing file.
        match fs::hard_link(&temporary, &path) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    fs::File::open(directory)
                        .and_then(|dir| dir.sync_all())
                        .map_err(|_| SystemError::Store)?;
                }
                eprintln!("event=system_bootstrapped account=root@localhost");
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                existing(directory)?;
                Ok(false)
            }
            Err(_) => Err(SystemError::Store),
        }
    })();
    let _ = fs::remove_file(&temporary);
    let _ = fs::remove_dir(&staging);
    result
}
fn strings(world: &World, id: EntityId, attr: &str) -> Result<Vec<String>, SystemError> {
    world
        .as_of(id, attr, world.latest_tick())
        .map_err(store)?
        .into_iter()
        .map(|value| match value {
            Value::Str(value) => Ok(value),
            _ => Err(SystemError::InvalidSystem),
        })
        .collect()
}
fn one(mut values: Vec<String>) -> Result<String, SystemError> {
    if values.len() != 1 {
        return Err(SystemError::InvalidSystem);
    }
    values.pop().ok_or(SystemError::InvalidSystem)
}
pub(crate) fn read_accounts(world: &World) -> Result<Vec<Account>, SystemError> {
    validate(world)?;
    let mut accounts = Vec::new();
    let mut seen = BTreeSet::new();
    for id in world.entities_of_type("ACCOUNT").map_err(store)? {
        let users = strings(world, id, "user")?;
        if users.is_empty() {
            continue;
        } // Dropped accounts remain in the audit history.
        let user = one(users)?;
        let host = one(strings(world, id, "host")?)?;
        let name = AccountName::parse(&format!("{user}@{host}"))?;
        if name.user != user || name.host != host || !seen.insert(name.key()) {
            return Err(SystemError::InvalidSystem);
        }
        let host_pattern = HostPattern::parse(&host)?;
        let plugin = match one(strings(world, id, "plugin")?)?.as_str() {
            "trust" => Plugin::Trust,
            "kala_password" => Plugin::KalaPassword,
            _ => return Err(SystemError::InvalidSystem),
        };
        let hashes = strings(world, id, "password_hash")?;
        let password = match plugin {
            Plugin::Trust if host_pattern.permits_trust() && hashes.is_empty() => None,
            Plugin::KalaPassword => Some(Credentials::new(one(hashes)?)),
            _ => return Err(SystemError::InvalidSystem),
        };
        let grants = strings(world, id, "grant")?;
        for grant in &grants {
            let (privilege, db) = grant.split_once(':').ok_or(SystemError::InvalidSystem)?;
            validate_grant(privilege, db)?;
        }
        accounts.push(Account {
            id,
            summary: AccountSummary {
                user,
                host,
                plugin,
                grants,
            },
            host: host_pattern,
            password,
        });
    }
    if !accounts
        .iter()
        .any(|a| a.summary.grants.iter().any(|g| g == "admin:*"))
    {
        return Err(SystemError::InvalidSystem);
    }
    accounts.sort_by(|a, b| {
        (&a.summary.user, &a.summary.host).cmp(&(&b.summary.user, &b.summary.host))
    });
    Ok(accounts)
}
/// Validate a restored accounts database, including all active accounts and
/// grants, without returning any password hashes to the caller.
pub fn validate_restore(world: &World) -> Result<(), SystemError> {
    read_accounts(world).map(|_| ())
}

/// List accounts, excluding all current and historical hashes.
pub fn list(directory: impl AsRef<Path>) -> Result<Vec<AccountSummary>, SystemError> {
    ensure(directory.as_ref())?;
    let world = World::open_read_only(&directory.as_ref().join("system.kala")).map_err(store)?;
    Ok(read_accounts(&world)?
        .into_iter()
        .map(|a| a.summary)
        .collect())
}
/// What a damaged last transaction of `system.kala` may do to a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Recovery {
    /// Refuse the write under the writer lock, before anything is cut
    /// ([`SystemError::RecoveryRequired`]).
    #[default]
    Refuse,
    /// Cut the damaged tail to the quarantine file, acknowledge the
    /// recovery and apply the write, as `kala transact --acknowledge-recovery`.
    Acknowledge,
}
/// The native result of one account change: how many entries it stored and
/// the tick it used (for "0 changed", the tick it was judged at).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteReport {
    /// Entries stored (0 for a no-op, which appends nothing).
    pub changed: usize,
    /// The tick the change was judged and written at.
    pub tick: Tick,
}
impl fmt::Display for WriteReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} changed, written at tick {}",
            self.changed, self.tick.0
        )
    }
}
/// One account-management change, applied by [`apply`].
#[derive(Clone, Copy)]
pub enum Change<'a> {
    /// Add an account; `None` selects trust (explicit loopback only).
    Add {
        /// `user@host`.
        account: &'a str,
        /// The new password, or `None` for trust.
        password: Option<&'a str>,
    },
    /// Change an account's authentication, keeping its grants.
    Set {
        /// `user@host`.
        account: &'a str,
        /// The new password, or `None` for trust.
        password: Option<&'a str>,
    },
    /// Grant `privilege:db`; an existing grant is "0 changed".
    Grant {
        /// `user@host`.
        account: &'a str,
        /// `read`, `write`, `backup` or `admin`.
        privilege: &'a str,
        /// A database name or `*`.
        db: &'a str,
    },
    /// Revoke `privilege:db`; an absent grant is "0 changed". The last
    /// `admin:*` grant cannot be revoked.
    Revoke {
        /// `user@host`.
        account: &'a str,
        /// `read`, `write`, `backup` or `admin`.
        privilege: &'a str,
        /// A database name or `*`.
        db: &'a str,
    },
    /// Drop an account by retracting its active fields (history is kept).
    Drop {
        /// `user@host`.
        account: &'a str,
    },
}
/// Never prints the password.
impl fmt::Debug for Change<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Add { account, password } | Self::Set { account, password } => f
                .debug_struct(if matches!(self, Self::Add { .. }) {
                    "Add"
                } else {
                    "Set"
                })
                .field("account", account)
                .field("password", &password.map(|_| "<redacted>"))
                .finish(),
            Self::Grant {
                account,
                privilege,
                db,
            }
            | Self::Revoke {
                account,
                privilege,
                db,
            } => f
                .debug_struct(if matches!(self, Self::Grant { .. }) {
                    "Grant"
                } else {
                    "Revoke"
                })
                .field("account", account)
                .field("privilege", privilege)
                .field("db", db)
                .finish(),
            Self::Drop { account } => f.debug_struct("Drop").field("account", account).finish(),
        }
    }
}
/// The outcome of [`apply`]: the change's result, and the damaged tail an
/// acknowledged recovery cut before the change was judged.
#[derive(Debug)]
pub struct Applied {
    /// Set only when [`Recovery::Acknowledge`] cut a damaged last
    /// transaction (fully present, checksum failed) to its quarantine file.
    /// Any account change in it (a password rotation, a revoke, a drop) is
    /// gone from the world, so the caller must tell the operator. Kept even
    /// when `result` is an error: the cut has already happened.
    pub recovered: Option<kala_core::RecoveryReport>,
    /// The change's own result.
    pub result: Result<WriteReport, SystemError>,
}
/// Apply one account change to `directory/system.kala` under its writer
/// lock, creating the system world (never the directory) if it is absent.
/// Waits for a lock another writer holds up to kala-core's default busy
/// timeout (5 s); see [`apply_waiting`].
pub fn apply(directory: impl AsRef<Path>, change: Change<'_>, recovery: Recovery) -> Applied {
    apply_waiting(
        directory,
        change,
        recovery,
        kala_core::log::DEFAULT_BUSY_TIMEOUT,
    )
}
/// [`apply`], waiting up to `busy_timeout` each time the system world's
/// writer lock is held by another writer (opening it, and committing the
/// change); zero means one attempt. Past it the result is
/// [`SystemError::Busy`] and nothing was written.
pub fn apply_waiting(
    directory: impl AsRef<Path>,
    change: Change<'_>,
    recovery: Recovery,
    busy_timeout: std::time::Duration,
) -> Applied {
    let directory = directory.as_ref();
    let mut open = Open {
        recovery,
        busy_timeout,
        recovered: None,
        conflicted: false,
    };
    // Every change is decided from the accounts as read (does the account
    // exist, is this the last admin, the next audit tick), so its
    // transaction counts as having read all of them: if another change is
    // committed first, kala-core refuses this one, having written nothing,
    // and it is decided again from the accounts as they are now. Bounded:
    // past it the caller sees `Busy` ("retry the command").
    const CONFLICT_RETRIES: usize = 16;
    let mut attempt = 0;
    let result = loop {
        open.conflicted = false;
        let result = match &change {
            Change::Add { account, password } => {
                add_inner(directory, account, *password, &mut open)
            }
            Change::Set { account, password } => {
                set_inner(directory, account, *password, &mut open)
            }
            Change::Grant {
                account,
                privilege,
                db,
            } => change_grant(directory, account, privilege, db, true, &mut open),
            Change::Revoke {
                account,
                privilege,
                db,
            } => change_grant(directory, account, privilege, db, false, &mut open),
            Change::Drop { account } => drop_inner(directory, account, &mut open),
        };
        if open.conflicted && attempt < CONFLICT_RETRIES {
            attempt += 1;
            continue;
        }
        break result;
    };
    Applied {
        recovered: open.recovered,
        result,
    }
}
/// How [`writer`] may recover the log, and what it cut.
struct Open {
    recovery: Recovery,
    /// How long to wait for the writer lock (see [`apply_waiting`]).
    busy_timeout: std::time::Duration,
    recovered: Option<kala_core::RecoveryReport>,
    /// The last attempt lost to another account change committed while it
    /// ran: nothing was written, and the change can be decided again.
    conflicted: bool,
}
/// Begins the transaction of an account change that was decided from
/// `world` as the caller just read it, writing at `tick`. The transaction
/// counts as having read every account, so a change committed by another
/// process before this one's commit is a conflict; and one that `begin`
/// itself caught up with (after the caller's reads) is one too.
fn begin<'w>(world: &'w mut World, tick: Tick, open: &mut Open) -> Result<Txn<'w>, SystemError> {
    let read_at = world.latest_tx();
    let mut txn = world.begin();
    if txn.snapshot_tx() != read_at {
        open.conflicted = true;
        return Err(SystemError::Busy);
    }
    txn.note_read_all();
    txn.set_default_tick(tick);
    Ok(txn)
}
/// Commits an account change begun with [`begin`].
fn commit(txn: Txn<'_>, open: &mut Open) -> Result<kala_core::CommitResult, SystemError> {
    txn.commit().map_err(|e| {
        open.conflicted = matches!(e, kala_core::Error::Conflict(_));
        store(e)
    })
}
/// Opens the system world for writing.
///
/// The open needs the writer lock, which another `kala user` command, or a
/// process that just closed the world, may hold for a moment (a process
/// spawning a child from another thread shares its open files with the
/// child until the child execs). kala-core waits for it up to the busy
/// timeout (5 s unless the caller chose another), which the opened world's
/// commits wait too; a writer that holds it longer is reported as
/// [`SystemError::Busy`].
fn open_writer(path: &Path, open: &Open) -> Result<World, SystemError> {
    match open.recovery {
        Recovery::Refuse => World::open_refusing_damaged_waiting(path, open.busy_timeout),
        Recovery::Acknowledge => World::open_waiting(path, open.busy_timeout),
    }
    .map_err(store)
}
fn writer(directory: &Path, open: &mut Open) -> Result<World, SystemError> {
    // `ensure` validates the committed prefix read-only, so a world that is
    // not a system world is refused before any recovery can cut its tail.
    ensure(directory)?;
    let path = directory.join("system.kala");
    let mut world = open_writer(&path, open)?;
    let damaged = world.recovery().cause == Some(kala_core::CutCause::Damaged);
    if damaged {
        // Reachable only with `Recovery::Acknowledge`: the refusing open
        // never cuts a damaged tail. Recorded before anything else can
        // fail, because the tail is already gone from the log.
        let report = world.recovery();
        eprintln!(
            "event=system_recovery_acknowledged bytes_cut={}",
            report.bytes_cut
        );
        open.recovered = Some(report.clone());
    }
    validate(&world)?;
    if damaged {
        world.acknowledge_recovery();
    }
    Ok(world)
}
fn next_tick(world: &World) -> Result<Tick, SystemError> {
    world
        .latest_tick()
        .0
        .checked_add(1)
        .map(Tick)
        .ok_or(SystemError::InvalidSystem)
}
fn authentication(
    name: &AccountName,
    password: Option<&str>,
) -> Result<(Plugin, Option<Credentials>), SystemError> {
    match password {
        Some(password) => Ok((Plugin::KalaPassword, Some(hash_password(password)?))),
        None if HostPattern::parse(&name.host)?.permits_trust() => Ok((Plugin::Trust, None)),
        None => Err(SystemError::TrustRequiresLoopback),
    }
}
fn find<'a>(accounts: &'a [Account], name: &AccountName) -> Option<&'a Account> {
    accounts
        .iter()
        .find(|a| a.summary.user == name.user && a.summary.host == name.host)
}
/// Add an account. `None` selects trust and is accepted only for explicit loopback.
/// A damaged last transaction is refused (see [`apply`] and [`Recovery`]).
pub fn add(
    directory: impl AsRef<Path>,
    account: &str,
    password: Option<&str>,
) -> Result<WriteReport, SystemError> {
    apply(
        directory,
        Change::Add { account, password },
        Recovery::Refuse,
    )
    .result
}
fn add_inner(
    directory: &Path,
    account: &str,
    password: Option<&str>,
    open: &mut Open,
) -> Result<WriteReport, SystemError> {
    let name = AccountName::parse(account)?;
    let (plugin, password) = authentication(&name, password)?;
    let mut world = writer(directory, open)?;
    if find(&read_accounts(&world)?, &name).is_some() {
        return Err(SystemError::AccountExists);
    }
    let key = name.key();
    let previous = world.entity_by_handle(&key);
    let tick = next_tick(&world)?;
    let mut txn = begin(&mut world, tick, open)?;
    // Reuse a dropped account's permanent handle, retaining its full history.
    let id = match previous {
        Some(id) => id,
        None => txn.create_entity("ACCOUNT", Some(&key)).map_err(store)?,
    };
    for (attr, value) in [
        ("user", name.user.as_str()),
        ("host", name.host.as_str()),
        ("plugin", &plugin.to_string()),
    ] {
        txn.assert(id, attr, Value::Str(value.into()), None)
            .map_err(store)?;
    }
    if let Some(hash) = password {
        txn.assert(id, "password_hash", Value::Str(hash.expose().into()), None)
            .map_err(store)?;
    }
    let committed = commit(txn, open)?;
    eprintln!(
        "event=account_added user={} host={} plugin={plugin}",
        name.user, name.host
    );
    Ok(WriteReport {
        changed: committed.changed,
        tick,
    })
}
fn replace(
    txn: &mut Txn<'_>,
    id: EntityId,
    attr: &str,
    old: Option<&str>,
    new: Option<&str>,
) -> Result<(), SystemError> {
    if old == new {
        return Ok(());
    }
    if let Some(old) = old {
        txn.retract(id, attr, Value::Str(old.into()), None)
            .map_err(store)?;
    }
    if let Some(new) = new {
        txn.assert(id, attr, Value::Str(new.into()), None)
            .map_err(store)?;
    }
    Ok(())
}
/// Change authentication while preserving grants and previous password history.
/// A damaged last transaction is refused (see [`apply`] and [`Recovery`]).
pub fn set(
    directory: impl AsRef<Path>,
    account: &str,
    password: Option<&str>,
) -> Result<WriteReport, SystemError> {
    apply(
        directory,
        Change::Set { account, password },
        Recovery::Refuse,
    )
    .result
}
fn set_inner(
    directory: &Path,
    account: &str,
    password: Option<&str>,
    open: &mut Open,
) -> Result<WriteReport, SystemError> {
    let name = AccountName::parse(account)?;
    let (plugin, password) = authentication(&name, password)?;
    let mut world = writer(directory, open)?;
    let accounts = read_accounts(&world)?;
    let old = find(&accounts, &name).ok_or(SystemError::AccountNotFound)?;
    let tick = next_tick(&world)?;
    let mut txn = begin(&mut world, tick, open)?;
    replace(
        &mut txn,
        old.id,
        "plugin",
        Some(&old.summary.plugin.to_string()),
        Some(&plugin.to_string()),
    )?;
    replace(
        &mut txn,
        old.id,
        "password_hash",
        old.password.as_ref().map(Credentials::expose),
        password.as_ref().map(Credentials::expose),
    )?;
    let committed = commit(txn, open)?;
    eprintln!(
        "event=account_auth_changed user={} host={} plugin={plugin} changed={}",
        name.user, name.host, committed.changed
    );
    Ok(WriteReport {
        changed: committed.changed,
        tick,
    })
}
fn validate_grant(privilege: &str, db: &str) -> Result<(), SystemError> {
    if !matches!(privilege, "read" | "write" | "backup" | "admin")
        || db.len() > 250
        || (db != "*"
            && (db.is_empty()
                || !db
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_-".contains(&c))))
    {
        return Err(SystemError::InvalidGrant);
    }
    Ok(())
}
fn guard_last_admin(accounts: &[Account], target: &Account) -> Result<(), SystemError> {
    if target.summary.grants.iter().any(|g| g == "admin:*")
        && accounts
            .iter()
            .filter(|a| a.summary.grants.iter().any(|g| g == "admin:*"))
            .count()
            == 1
    {
        return Err(SystemError::LastAdmin);
    }
    Ok(())
}
/// Grant a privilege; granting an existing privilege writes nothing.
/// A damaged last transaction is refused (see [`apply`] and [`Recovery`]).
pub fn grant(
    directory: impl AsRef<Path>,
    account: &str,
    privilege: &str,
    db: &str,
) -> Result<WriteReport, SystemError> {
    apply(
        directory,
        Change::Grant {
            account,
            privilege,
            db,
        },
        Recovery::Refuse,
    )
    .result
}
/// Revoke a privilege; the last `admin:*` grant cannot be revoked.
/// A damaged last transaction is refused (see [`apply`] and [`Recovery`]).
pub fn revoke(
    directory: impl AsRef<Path>,
    account: &str,
    privilege: &str,
    db: &str,
) -> Result<WriteReport, SystemError> {
    apply(
        directory,
        Change::Revoke {
            account,
            privilege,
            db,
        },
        Recovery::Refuse,
    )
    .result
}
fn change_grant(
    directory: &Path,
    account: &str,
    privilege: &str,
    db: &str,
    add: bool,
    open: &mut Open,
) -> Result<WriteReport, SystemError> {
    let name = AccountName::parse(account)?;
    validate_grant(privilege, db)?;
    let mut world = writer(directory, open)?;
    let accounts = read_accounts(&world)?;
    let target = find(&accounts, &name).ok_or(SystemError::AccountNotFound)?;
    let grant = format!("{privilege}:{db}");
    let tick = next_tick(&world)?;
    if add == target.summary.grants.contains(&grant) {
        // Asserting a held grant or retracting an absent one: "0 changed",
        // judged at the tick the write would have used. Nothing is appended.
        return Ok(WriteReport { changed: 0, tick });
    }
    if !add && grant == "admin:*" {
        guard_last_admin(&accounts, target)?;
    }
    let mut txn = begin(&mut world, tick, open)?;
    if add {
        txn.assert(target.id, "grant", Value::Str(grant), None)
            .map_err(store)?;
    } else {
        txn.retract(target.id, "grant", Value::Str(grant), None)
            .map_err(store)?;
    }
    let committed = commit(txn, open)?;
    eprintln!(
        "event=account_grant_changed user={} host={} privilege={privilege} db={db} granted={add}",
        name.user, name.host
    );
    Ok(WriteReport {
        changed: committed.changed,
        tick,
    })
}
/// Drop an account by retracting its active fields, preserving audit history.
/// A damaged last transaction is refused (see [`apply`] and [`Recovery`]).
pub fn drop_account(
    directory: impl AsRef<Path>,
    account: &str,
) -> Result<WriteReport, SystemError> {
    apply(directory, Change::Drop { account }, Recovery::Refuse).result
}
fn drop_inner(
    directory: &Path,
    account: &str,
    open: &mut Open,
) -> Result<WriteReport, SystemError> {
    let name = AccountName::parse(account)?;
    let mut world = writer(directory, open)?;
    let accounts = read_accounts(&world)?;
    let target = find(&accounts, &name).ok_or(SystemError::AccountNotFound)?;
    guard_last_admin(&accounts, target)?;
    let values = ATTRS
        .into_iter()
        .map(|attr| strings(&world, target.id, attr).map(|v| (attr, v)))
        .collect::<Result<Vec<_>, _>>()?;
    let tick = next_tick(&world)?;
    let mut txn = begin(&mut world, tick, open)?;
    for (attr, values) in values {
        for value in values {
            txn.retract(target.id, attr, Value::Str(value), None)
                .map_err(store)?;
        }
    }
    let committed = commit(txn, open)?;
    eprintln!(
        "event=account_dropped user={} host={}",
        name.user, name.host
    );
    Ok(WriteReport {
        changed: committed.changed,
        tick,
    })
}
