//! The only place the binary writes to the terminal.

use std::io::Write as _;

/// Writes the Dockerfile for the plan step's log, then the info object as the last line.
pub(crate) fn plan(dockerfile: Option<&str>, info: &str) -> Result<(), String> {
    let mut out = std::io::stdout().lock();
    if let Some(dockerfile) = dockerfile {
        writeln!(out, "{}", dockerfile.trim_end()).map_err(|e| e.to_string())?;
    }
    writeln!(out, "{info}")
        .and_then(|()| out.flush())
        .map_err(|e| e.to_string())
}

/// Writes an error of the agent's own to stderr.
pub(crate) fn error(error: &str) {
    let mut err = std::io::stderr().lock();
    writeln!(err, "zeabur-builder-agent: {error}").unwrap_or_default();
}
