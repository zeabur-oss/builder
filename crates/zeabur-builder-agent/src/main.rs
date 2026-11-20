//! `zeabur-builder-agent`: writes a Dockerfile for a repository and trial-builds it.

#![doc = include_str!("../README.md")]

mod agent;
mod budget;
mod federation;
mod http;
mod output;
mod plan;
mod repo;
mod run;
#[cfg(test)]
mod testing;
mod trial;

use std::process::ExitCode;

use clap::Parser as _;

#[tokio::main]
async fn main() -> ExitCode {
    let args = run::Args::parse();
    match run::run(args).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(2),
        Err(e) => {
            output::error(&e.to_string());
            ExitCode::FAILURE
        }
    }
}
