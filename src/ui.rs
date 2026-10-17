use std::fmt::Display;

/// Status line on stderr, so a program's stdout stays clean for piping.
pub fn status(quiet: bool, message: impl Display) {
    if !quiet {
        eprintln!("pyrs: {message}");
    }
}

pub fn warn(message: impl Display) {
    eprintln!("pyrs: warning: {message}");
}
