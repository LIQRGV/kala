//! Opt-in history: bounded input and same-handle I/O, never a path-based
//! truncate. Terminal history APIs intentionally have no filesystem access.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use kala_datalog::MAX_QUERY_BYTES;

use crate::error::CliError;

pub(super) const MAX_HISTORY_BYTES: usize = 1024 * 1024;
pub(super) const MAX_HISTORY_ENTRIES: usize = 1000;

/// Rustyline redraws recalled text verbatim. History is data from disk or
/// stdin, so never let it carry terminal controls or invisible bidi marks.
pub(super) fn safe_entry(text: &str) -> bool {
    !text.chars().any(kala_core::must_escape)
}

pub(super) struct History {
    file: File,
    entries: VecDeque<String>,
    bytes: usize,
}

impl History {
    pub(super) fn open(path: &Path, world: &Path) -> Result<Self, CliError> {
        fn io(e: impl std::fmt::Display) -> CliError {
            CliError::User(format!("history: {e}"))
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        // On non-Unix platforms std does not expose no-follow opens. Refuse
        // existing paths there instead of silently weakening the guarantee.
        let file = match options.create_new(true).open(path) {
            Ok(file) => file,
            #[cfg(unix)]
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                options.create_new(false).open(path).map_err(io)?
            }
            Err(e) => return Err(io(e)),
        };
        let meta = file.metadata().map_err(io)?;
        if !meta.is_file() {
            return Err(io("not a regular file"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let world_meta = std::fs::metadata(world).map_err(io)?;
            if meta.nlink() != 1
                || (meta.dev() == world_meta.dev() && meta.ino() == world_meta.ino())
            {
                return Err(io("history must not alias the world or have hard links"));
            }
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(io)?;
        }
        #[cfg(not(unix))]
        let _ = world;
        if meta.len() > MAX_HISTORY_BYTES as u64 {
            return Err(io("history exceeds 1 MiB"));
        }
        let mut history = Self {
            file,
            entries: VecDeque::new(),
            bytes: 0,
        };
        let mut contents = Vec::new();
        (&mut history.file)
            .take(MAX_HISTORY_BYTES as u64 + 1)
            .read_to_end(&mut contents)
            .map_err(io)?;
        if contents.len() > MAX_HISTORY_BYTES {
            return Err(io("history exceeds 1 MiB"));
        }
        let text = String::from_utf8(contents).map_err(io)?;
        for line in text.lines() {
            // One JSON string per line preserves multiline queries without
            // granting history files any command-execution semantics.
            let entry: String = serde_json::from_str(line).map_err(io)?;
            if entry.len() > MAX_QUERY_BYTES {
                return Err(io("history entry exceeds the query limit"));
            }
            if !safe_entry(&entry) {
                return Err(io(
                    "history entry contains unsafe control or formatting characters",
                ));
            }
            history.push(&entry);
        }
        Ok(history)
    }

    pub(super) fn entries(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(String::as_str)
    }

    pub(super) fn push(&mut self, text: &str) {
        if text.trim().is_empty() || text.len() > MAX_QUERY_BYTES || !safe_entry(text) {
            return;
        }
        // JSON escapes can expand each byte to six bytes, plus quotes/newline.
        let cost = text.len().saturating_mul(6).saturating_add(3);
        while self.bytes + cost > MAX_HISTORY_BYTES || self.entries.len() >= MAX_HISTORY_ENTRIES {
            let Some(old) = self.entries.pop_front() else {
                return;
            };
            self.bytes -= old.len() * 6 + 3;
        }
        self.bytes += cost;
        self.entries.push_back(text.to_owned());
    }

    pub(super) fn save(&mut self) -> Result<(), CliError> {
        fn io(e: impl std::fmt::Display) -> CliError {
            CliError::Store(format!("saving history: {e}"))
        }
        // Recheck hard links before a write, and never reopen by path.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if self.file.metadata().map_err(io)?.nlink() != 1 {
                return Err(io("history acquired a hard link or was removed"));
            }
        }
        // Encode everything first, so a failure can't leave a half-written
        // file; then replace the contents in one write.
        let mut buf = Vec::with_capacity(self.bytes + 4 * self.entries.len());
        for entry in &self.entries {
            serde_json::to_writer(&mut buf, entry).map_err(io)?;
            buf.push(b'\n');
        }
        self.file.seek(SeekFrom::Start(0)).map_err(io)?;
        self.file.set_len(0).map_err(io)?;
        self.file.write_all(&buf).map_err(io)?;
        self.file.flush().map_err(io)
    }
}
