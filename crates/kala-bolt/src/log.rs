//! Event lines on stderr. A log write must never cost more than the line:
//! `eprintln!` panics when stderr is gone (a closed terminal gives EIO, a
//! dead log pipe EPIPE), which would end a session thread, the accept loop or
//! signal handling. These helpers ignore the error instead.
use std::fmt;
use std::io::{self, Write};
use std::time::{Duration, Instant};

/// Write one line to stderr in a single `write`, ignoring any failure.
pub(crate) fn line(args: fmt::Arguments<'_>) {
    let mut text = args.to_string();
    text.push('\n');
    let _ = io::stderr().lock().write_all(text.as_bytes());
}

/// `eprintln!` that cannot panic.
macro_rules! logln {
    ($($arg:tt)*) => {
        $crate::log::line(format_args!($($arg)*))
    };
}
pub(crate) use logln;

/// Coalesces a repeating log event: the first occurrence in each second is
/// printed, later ones are counted and reported once with the next print,
/// so a client looping connect/close can't flood stderr.
pub(crate) struct LogLimit {
    last: Option<Instant>,
    suppressed: u64,
}

impl LogLimit {
    pub(crate) const fn new() -> Self {
        Self {
            last: None,
            suppressed: 0,
        }
    }

    pub(crate) fn log(&mut self, line: fmt::Arguments<'_>) {
        let now = Instant::now();
        if self
            .last
            .is_some_and(|t| now.duration_since(t) < Duration::from_secs(1))
        {
            self.suppressed += 1;
            return;
        }
        if self.suppressed > 0 {
            logln!("{line} suppressed_since_last={}", self.suppressed);
        } else {
            logln!("{line}");
        }
        self.last = Some(now);
        self.suppressed = 0;
    }
}
