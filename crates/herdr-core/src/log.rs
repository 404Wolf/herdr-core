//! The core's log. Stderr (core.log, the holder's log) can fail to take a
//! line: a full disk, a closed pipe. `eprintln!` panics then, and a panic in
//! the registry writer or in a holder thread ends it for good. A log line is
//! never worth that, so the core and the holder log with this and lose the
//! line when it can't be written.

/// `eprintln!` that never panics: a line that can't be written is dropped.
#[macro_export]
#[doc(hidden)]
macro_rules! log {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}
