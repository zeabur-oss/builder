//! The only place the binary writes to the terminal.

use std::io::Write as _;

/// Why the plan could not be written to stdout.
#[derive(Debug, thiserror::Error)]
#[error("cannot write to stdout: {0}")]
pub(crate) struct WriteError(#[from] std::io::Error);

/// Writes the Dockerfile for the plan step's log, then the info object as the last line.
pub(crate) fn plan(dockerfile: Option<&str>, info: &str) -> Result<(), WriteError> {
    let mut out = std::io::stdout().lock();
    if let Some(dockerfile) = dockerfile {
        writeln!(out, "{}", dockerfile.trim_end())?;
    }
    writeln!(out, "{info}")?;
    Ok(out.flush()?)
}

/// Writes an error of the agent's own to stderr.
pub(crate) fn error(error: &str) {
    let mut err = std::io::stderr().lock();
    writeln!(err, "zeabur-builder-agent: {error}").unwrap_or_default();
}
