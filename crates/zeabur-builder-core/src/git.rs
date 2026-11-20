//! Resolving a branch or tag to a commit over Git's smart HTTP protocol, without Git.
//!
//! The controller asks the server itself (a webhook never supplies the commit)
//! and needs only two requests ([http-protocol], [protocol-v2]):
//!
//! 1. `GET <url>/info/refs?service=git-upload-pack` with `Git-Protocol: version=2`. A server that
//!    speaks version 2 answers with its capabilities; an older one ignores the header and lists
//!    every ref (version 0), which is already the answer.
//! 2. For version 2, `POST <url>/git-upload-pack` with an `ls-refs` command limited to the
//!    candidate names, so a repository with thousands of refs answers with two lines.
//!
//! Both answers are pkt-lines ([pkt-line]): four hexadecimal digits giving the length including
//! themselves, then the payload; `0000` ends a section (flush), `0001` separates arguments
//! (delimiter), `0002` ends a response.
//!
//! A short name is looked up as a branch first, then as a tag. `git rev-parse` prefers tags, but a
//! `Build` that follows a ref almost always follows a branch, and a tag named like a branch is
//! more likely a mistake than an intent. A full name (`refs/…`) is taken as written. Annotated
//! tags resolve to the commit they point at (the peeled object), since that is what `fetch`
//! checks out.
//!
//! [`DEFAULT_BRANCH`] (`HEAD`) follows the remote's default branch, as `git clone` does. The
//! server says which branch `HEAD` points at: `symref=HEAD:<name>` among the capabilities of a
//! version 0 advertisement ([capabilities]), `symref-target:<name>` when `ls-refs` asks for
//! `symrefs` in version 2. A server that does not say still gives `HEAD`'s commit.
//!
//! [http-protocol]: https://git-scm.com/docs/http-protocol
//! [protocol-v2]: https://git-scm.com/docs/protocol-v2#_ls_refs
//! [pkt-line]: https://git-scm.com/docs/protocol-common#_pkt_line_format
//! [capabilities]: https://git-scm.com/docs/gitprotocol-capabilities#_symref

use thiserror::Error;

use crate::is_commit_sha;

/// The longest response read, to bound memory on a hostile or huge server.
pub const MAX_RESPONSE_BYTES: usize = 16 << 20;

/// The ref that follows the remote's default branch.
pub const DEFAULT_BRANCH: &str = "HEAD";

/// One ref as the server reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRef {
    /// The full name, such as `refs/heads/main`.
    pub name: String,
    /// The object it points at.
    pub oid: String,
    /// For an annotated tag, the commit the tag points at.
    pub peeled: Option<String>,
    /// For a symbolic ref such as `HEAD`, the ref it points at.
    pub symref_target: Option<String>,
}

impl RemoteRef {
    /// The commit: the peeled object for an annotated tag, else the object itself.
    #[must_use]
    pub fn commit(&self) -> &str {
        self.peeled.as_deref().unwrap_or(&self.oid)
    }
}

/// A resolved ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The commit.
    pub commit: String,
    /// The full name of the ref the commit came from: the branch `HEAD` points at when the
    /// server says, else the matching candidate.
    pub name: String,
}

/// What the capability request returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Advertisement {
    /// The server speaks protocol version 2; ask with [`ls_refs_request`].
    V2,
    /// The server listed every ref (protocol version 0 or 1).
    Refs(Vec<RemoteRef>),
}

/// Why a ref could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GitError {
    /// The response is not a sequence of pkt-lines.
    #[error("the server's response is not in pkt-line format")]
    PktLine,
    /// A line is not `<oid> <name>`.
    #[error("the server sent an unexpected ref line: {0}")]
    RefLine(String),
    /// The server reported an error (`ERR …`).
    #[error("the server reported: {0}")]
    Server(String),
    /// The response is larger than [`MAX_RESPONSE_BYTES`].
    #[error("the server's response is larger than {MAX_RESPONSE_BYTES} bytes")]
    TooLarge,
    /// No branch or tag has that name.
    #[error("no branch or tag `{0}`")]
    RefNotFound(String),
    /// The repository has no default branch, as when it is empty.
    #[error("the repository has no default branch")]
    NoDefaultBranch,
}

/// One pkt-line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pkt<'a> {
    Data(&'a [u8]),
    Flush,
    Delim,
    ResponseEnd,
}

fn pkt_lines(mut body: &[u8]) -> Result<Vec<Pkt<'_>>, GitError> {
    if body.len() > MAX_RESPONSE_BYTES {
        return Err(GitError::TooLarge);
    }
    let mut lines = Vec::new();
    while !body.is_empty() {
        let (len, rest) = body.split_at_checked(4).ok_or(GitError::PktLine)?;
        let len = std::str::from_utf8(len)
            .ok()
            .and_then(|hex| usize::from_str_radix(hex, 16).ok())
            .ok_or(GitError::PktLine)?;
        match len {
            0 => lines.push(Pkt::Flush),
            1 => lines.push(Pkt::Delim),
            2 => lines.push(Pkt::ResponseEnd),
            3 => return Err(GitError::PktLine),
            _ => {
                let (data, after) = rest.split_at_checked(len - 4).ok_or(GitError::PktLine)?;
                if let Some(message) = data.strip_prefix(b"ERR ") {
                    return Err(GitError::Server(text(message)));
                }
                lines.push(Pkt::Data(data));
                body = after;
                continue;
            }
        }
        body = rest;
    }
    Ok(lines)
}

fn text(data: &[u8]) -> String {
    String::from_utf8_lossy(data)
        .trim_end_matches('\n')
        .to_owned()
}

fn pkt(out: &mut Vec<u8>, line: &str) {
    out.extend_from_slice(format!("{:04x}", line.len() + 4).as_bytes());
    out.extend_from_slice(line.as_bytes());
}

/// Reads the answer to `GET info/refs?service=git-upload-pack`.
///
/// # Errors
///
/// When the body is not pkt-lines, a ref line is malformed, or the server reported an error.
pub fn parse_advertisement(body: &[u8]) -> Result<Advertisement, GitError> {
    let lines = pkt_lines(body)?;
    let mut data = lines.iter().filter_map(|pkt| match pkt {
        Pkt::Data(d) => Some(*d),
        Pkt::Flush | Pkt::Delim | Pkt::ResponseEnd => None,
    });
    let mut first = data.next();
    // Smart HTTP prefixes `# service=git-upload-pack` and a flush.
    if first.is_some_and(|d| d.starts_with(b"# service=")) {
        first = data.next();
    }
    if first.is_some_and(|d| text(d) == "version 2") {
        return Ok(Advertisement::V2);
    }
    let mut refs: Vec<RemoteRef> = Vec::new();
    let mut head_target = None;
    for line in first.into_iter().chain(data) {
        // The first ref carries the capabilities after a NUL.
        let mut parts = line.splitn(2, |&b| b == 0);
        let line = text(parts.next().unwrap_or_default());
        if let Some(capabilities) = parts.next() {
            head_target = text(capabilities)
                .split(' ')
                .find_map(|c| c.strip_prefix("symref=HEAD:"))
                .map(str::to_owned);
        }
        let (oid, name) = line
            .split_once(' ')
            .filter(|(oid, _)| is_commit_sha(oid))
            .ok_or_else(|| GitError::RefLine(line.clone()))?;
        if let Some(tag) = name.strip_suffix("^{}") {
            if let Some(r) = refs.iter_mut().find(|r| r.name == tag) {
                r.peeled = Some(oid.to_owned());
            }
            continue;
        }
        // An empty repository advertises `capabilities^{}` with a zero id.
        if name == "capabilities^{}" {
            continue;
        }
        refs.push(RemoteRef {
            name: name.to_owned(),
            oid: oid.to_owned(),
            peeled: None,
            symref_target: (name == DEFAULT_BRANCH)
                .then(|| head_target.clone())
                .flatten(),
        });
    }
    Ok(Advertisement::Refs(refs))
}

/// Where a version 0 advertisement read from a stream ends: its length up to and including the
/// first flush, or `None` while more must be read. Over SSH the stream does not end by itself.
///
/// # Errors
///
/// When the bytes so far are not pkt-lines, exceed [`MAX_RESPONSE_BYTES`], or carry an error.
pub fn advertisement_end(body: &[u8]) -> Result<Option<usize>, GitError> {
    let mut at = 0;
    loop {
        if at > MAX_RESPONSE_BYTES {
            return Err(GitError::TooLarge);
        }
        let Some(head) = body.get(at..at + 4) else {
            return Ok(None);
        };
        let len = std::str::from_utf8(head)
            .ok()
            .and_then(|hex| usize::from_str_radix(hex, 16).ok())
            .ok_or(GitError::PktLine)?;
        match len {
            0 => return Ok(Some(at + 4)),
            1..=3 => return Err(GitError::PktLine),
            _ => {
                let Some(data) = body.get(at + 4..at + len) else {
                    return Ok(None);
                };
                if let Some(message) = data.strip_prefix(b"ERR ") {
                    return Err(GitError::Server(text(message)));
                }
                at += len;
            }
        }
    }
}

/// The names a ref may stand for, in preference order.
#[must_use]
pub fn candidates(git_ref: &str) -> Vec<String> {
    if git_ref.starts_with("refs/") || git_ref == DEFAULT_BRANCH {
        vec![git_ref.to_owned()]
    } else {
        vec![
            format!("refs/heads/{git_ref}"),
            format!("refs/tags/{git_ref}"),
        ]
    }
}

/// The body of a protocol version 2 `ls-refs` request for the candidates of `git_ref`.
#[must_use]
pub fn ls_refs_request(git_ref: &str) -> Vec<u8> {
    let mut out = Vec::new();
    pkt(&mut out, "command=ls-refs\n");
    out.extend_from_slice(b"0001");
    pkt(&mut out, "peel\n");
    pkt(&mut out, "symrefs\n");
    for name in candidates(git_ref) {
        pkt(&mut out, &format!("ref-prefix {name}\n"));
    }
    out.extend_from_slice(b"0000");
    out
}

/// Reads the answer to [`ls_refs_request`].
///
/// # Errors
///
/// When the body is not pkt-lines, a ref line is malformed, or the server reported an error.
pub fn parse_ls_refs(body: &[u8]) -> Result<Vec<RemoteRef>, GitError> {
    let mut refs = Vec::new();
    for pkt in pkt_lines(body)? {
        let Pkt::Data(data) = pkt else { continue };
        let line = text(data);
        let mut words = line.split(' ');
        let (Some(oid), Some(name)) = (words.next(), words.next()) else {
            return Err(GitError::RefLine(line));
        };
        if !is_commit_sha(oid) {
            return Err(GitError::RefLine(line));
        }
        let mut peeled = None;
        let mut symref_target = None;
        for attr in words {
            if let Some(oid) = attr
                .strip_prefix("peeled:")
                .filter(|oid| is_commit_sha(oid))
            {
                peeled = Some(oid.to_owned());
            } else if let Some(target) = attr.strip_prefix("symref-target:") {
                symref_target = Some(target.to_owned());
            }
        }
        refs.push(RemoteRef {
            name: name.to_owned(),
            oid: oid.to_owned(),
            peeled,
            symref_target,
        });
    }
    Ok(refs)
}

/// The commit `git_ref` points at, and the ref it came from.
///
/// # Errors
///
/// [`GitError::RefNotFound`] when no candidate name is among `refs`;
/// [`GitError::NoDefaultBranch`] for [`DEFAULT_BRANCH`] when the server sent no `HEAD`.
pub fn resolve(git_ref: &str, refs: &[RemoteRef]) -> Result<Resolved, GitError> {
    let Some(found) = candidates(git_ref)
        .iter()
        .find_map(|name| refs.iter().find(|r| &r.name == name))
    else {
        return Err(if git_ref == DEFAULT_BRANCH {
            GitError::NoDefaultBranch
        } else {
            GitError::RefNotFound(git_ref.to_owned())
        });
    };
    Ok(Resolved {
        commit: found.commit().to_owned(),
        name: found
            .symref_target
            .clone()
            .unwrap_or_else(|| found.name.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "3e1a9c2d4f0b5a6c7d8e9f0a1b2c3d4e5f6a7b8c";
    const B: &str = "0123456789abcdef0123456789abcdef01234567";
    const C: &str = "89abcdef0123456789abcdef0123456789abcdef";

    fn sha(git_ref: &str, refs: &[RemoteRef]) -> Result<String, GitError> {
        resolve(git_ref, refs).map(|r| r.commit)
    }

    fn pkts(lines: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for line in lines {
            match *line {
                "0000" | "0001" | "0002" => out.extend_from_slice(line.as_bytes()),
                _ => pkt(&mut out, line),
            }
        }
        out
    }

    #[test]
    fn advertisement_over_a_stream() {
        let mut body = pkts(&[
            &format!("{A} HEAD\0multi_ack\n"),
            &format!("{A} refs/heads/main\n"),
            "0000",
        ]);
        let end = body.len();
        body.extend_from_slice(b"0009more");
        assert_eq!(advertisement_end(&body), Ok(Some(end)));
        for cut in [0, 3, 4, 20, end - 1] {
            assert_eq!(advertisement_end(&body[..cut]), Ok(None), "{cut}");
        }
        let Ok(Advertisement::Refs(refs)) = parse_advertisement(&body[..end]) else {
            panic!("not refs");
        };
        assert_eq!(sha("main", &refs), Ok(A.to_owned()));

        let denied = pkts(&["ERR access denied\n"]);
        assert_eq!(
            advertisement_end(&denied),
            Err(GitError::Server("access denied".to_owned()))
        );
        assert_eq!(advertisement_end(b"00zz"), Err(GitError::PktLine));
        assert_eq!(advertisement_end(b"0003"), Err(GitError::PktLine));
    }

    #[test]
    fn version_2_capabilities() {
        let body = pkts(&[
            "# service=git-upload-pack\n",
            "0000",
            "version 2\n",
            "agent=git/github-g0\n",
            "ls-refs=unborn\n",
            "fetch=shallow wait-for-done filter\n",
            "0000",
        ]);
        assert_eq!(parse_advertisement(&body), Ok(Advertisement::V2));
    }

    #[test]
    fn version_0_lists_every_ref() {
        let first = format!("{A} HEAD\0multi_ack symref=HEAD:refs/heads/main\n");
        let body = pkts(&[
            "# service=git-upload-pack\n",
            "0000",
            &first,
            &format!("{A} refs/heads/main\n"),
            &format!("{B} refs/tags/v1\n"),
            &format!("{C} refs/tags/v1^{{}}\n"),
            "0000",
        ]);
        let Ok(Advertisement::Refs(refs)) = parse_advertisement(&body) else {
            panic!("refs expected")
        };
        assert_eq!(sha("main", &refs), Ok(A.to_owned()));
        assert_eq!(sha("v1", &refs), Ok(C.to_owned()), "peeled");
        assert_eq!(sha("refs/tags/v1", &refs), Ok(C.to_owned()));
        assert_eq!(
            sha("dev", &refs),
            Err(GitError::RefNotFound("dev".to_owned()))
        );
    }

    #[test]
    fn empty_repository() {
        let zero = "0".repeat(40);
        let body = pkts(&[&format!("{zero} capabilities^{{}}\0agent=x\n"), "0000"]);
        assert_eq!(parse_advertisement(&body), Ok(Advertisement::Refs(vec![])));
    }

    #[test]
    fn ls_refs_round_trip() {
        let request = ls_refs_request("main");
        assert_eq!(
            String::from_utf8(request).unwrap(),
            "0014command=ls-refs\n00010009peel\n000csymrefs\n\
             001fref-prefix refs/heads/main\n001eref-prefix refs/tags/main\n0000"
        );
        let body = pkts(&[
            &format!("{B} refs/tags/main peeled:{C}\n"),
            &format!("{A} refs/heads/main\n"),
            "0000",
        ]);
        let refs = parse_ls_refs(&body).unwrap();
        assert_eq!(sha("main", &refs), Ok(A.to_owned()), "branch first");
        assert_eq!(sha("refs/tags/main", &refs), Ok(C.to_owned()));
    }

    #[test]
    fn rejects_malformed_responses() {
        assert_eq!(parse_ls_refs(b"zzzz"), Err(GitError::PktLine));
        assert_eq!(parse_ls_refs(b"0003"), Err(GitError::PktLine));
        assert_eq!(parse_ls_refs(b"0010abc"), Err(GitError::PktLine));
        assert_eq!(
            parse_ls_refs(&pkts(&["ERR access denied\n"])),
            Err(GitError::Server("access denied".to_owned()))
        );
        assert!(matches!(
            parse_ls_refs(&pkts(&["main refs/heads/main\n"])),
            Err(GitError::RefLine(_))
        ));
        assert!(matches!(
            parse_advertisement(&pkts(&["<html>\n"])),
            Err(GitError::RefLine(_))
        ));
        assert_eq!(
            parse_ls_refs(&vec![b'0'; MAX_RESPONSE_BYTES + 1]),
            Err(GitError::TooLarge)
        );
    }

    #[test]
    fn default_branch_over_version_0() {
        let first = format!("{A} HEAD\0multi_ack symref=HEAD:refs/heads/master agent=git/2\n");
        let body = pkts(&[
            &first,
            &format!("{A} refs/heads/master\n"),
            &format!("{B} refs/heads/main\n"),
            "0000",
        ]);
        let Ok(Advertisement::Refs(refs)) = parse_advertisement(&body) else {
            panic!("refs expected")
        };
        assert_eq!(
            resolve(DEFAULT_BRANCH, &refs),
            Ok(Resolved {
                commit: A.to_owned(),
                name: "refs/heads/master".to_owned()
            })
        );
        assert_eq!(
            resolve("main", &refs),
            Ok(Resolved {
                commit: B.to_owned(),
                name: "refs/heads/main".to_owned()
            })
        );

        let detached = pkts(&[&format!("{C} HEAD\0multi_ack\n"), "0000"]);
        let Ok(Advertisement::Refs(refs)) = parse_advertisement(&detached) else {
            panic!("refs expected")
        };
        assert_eq!(
            resolve(DEFAULT_BRANCH, &refs),
            Ok(Resolved {
                commit: C.to_owned(),
                name: "HEAD".to_owned()
            })
        );
        assert_eq!(resolve(DEFAULT_BRANCH, &[]), Err(GitError::NoDefaultBranch));
    }

    #[test]
    fn default_branch_over_version_2() {
        assert_eq!(candidates(DEFAULT_BRANCH), ["HEAD"]);
        assert_eq!(
            String::from_utf8(ls_refs_request(DEFAULT_BRANCH)).unwrap(),
            "0014command=ls-refs\n00010009peel\n000csymrefs\n0014ref-prefix HEAD\n0000"
        );
        let body = pkts(&[
            &format!("{A} HEAD symref-target:refs/heads/master\n"),
            "0000",
        ]);
        let refs = parse_ls_refs(&body).unwrap();
        assert_eq!(
            resolve(DEFAULT_BRANCH, &refs),
            Ok(Resolved {
                commit: A.to_owned(),
                name: "refs/heads/master".to_owned()
            })
        );
    }
}
