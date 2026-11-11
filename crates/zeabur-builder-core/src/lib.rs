//! Zeabur Builder's decisions that need no I/O.
//!
//! - [`archive`]: the uploaded-archive source, its OCI artifact and the tar entries it may hold.
//! - [`git`]: resolving a branch or tag to a commit from a Git server's answer.
//! - [`ssh`]: SSH remotes and the `known_hosts` keys that vouch for them.
//! - [`hook`]: the repository and refs a webhook delivery names.
//! - [`repo`]: one key per Git repository, however its URL is written, so a webhook, a poll and a
//!   `Build` agree on which repository they mean.
//! - [`tag`]: the immutable, sortable image tag of a build.
//! - [`daemon`]: the name and address of a build namespace's own buildkitd.
//! - [`result`]: the build result a Job reports in its termination message.
//! - [`progress`]: reading `buildctl --progress rawjson` output, its cached steps and its steps.
//! - [`railpack`]: making a Railpack image run as a non-root user.
//!
//! The controller and the Job image both depend on this crate, which is why it stays free of I/O,
//! clocks and randomness: the same inputs always give the same decision.

pub mod archive;
pub mod daemon;
pub mod git;
pub mod hook;
pub mod progress;
pub mod railpack;
pub mod repo;
pub mod result;
pub mod ssh;
pub mod tag;

/// A full Git commit SHA: 40 lowercase hexadecimal digits.
///
/// SHA-256 repositories (64 digits) are not supported yet: GitHub does not host them.
#[must_use]
pub fn is_commit_sha(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_sha() {
        assert!(is_commit_sha("3e1a9c2d4f0b5a6c7d8e9f0a1b2c3d4e5f6a7b8c"));
        assert!(!is_commit_sha("3E1A9C2D4F0B5A6C7D8E9F0A1B2C3D4E5F6A7B8C"));
        assert!(!is_commit_sha("3e1a9c2"));
        assert!(!is_commit_sha("main"));
    }
}
