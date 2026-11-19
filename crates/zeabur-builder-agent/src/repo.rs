//! The read-only view of the repository the agent's `list`, `read` and `grep` tools give.
//!
//! Every path is relative to the context and resolves inside it, symbolic links included. Hidden
//! entries (version control, dependency directories, credentials) are neither listed, read nor
//! searched, and [`BUILD_IGNORE`] keeps the credentials out of every build context too. Each result
//! is at most [`MAX_RESULT`] bytes and says how to read on.

use std::fmt::Write as _;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// The largest result of one call.
pub(crate) const MAX_RESULT: usize = 12 * 1024;
/// Files larger than this are not searched.
const MAX_GREP_FILE: u64 = 1 << 20;
/// The deepest `list` goes.
const MAX_DEPTH: u32 = 4;

/// Directories no tool shows: version control, installed dependencies, caches.
const HIDDEN_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "bower_components",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".next",
    ".nuxt",
    ".turbo",
    ".cache",
    "target",
    "vendor",
];

/// `.dockerignore` rules every build starts with, so a `COPY . .` never takes credentials.
pub(crate) const BUILD_IGNORE: &str = "\
**/.git
**/.env
**/.env.*
!**/.env.example
!**/.env.sample
!**/.env.template
**/.npmrc
**/.netrc
**/.pypirc
**/.git-credentials
**/*.pem
**/*.key
**/id_rsa*
**/id_ecdsa*
**/id_ed25519*
";

/// Whether a file name holds credentials, matching [`BUILD_IGNORE`].
fn secret_file(name: &str) -> bool {
    let env = name == ".env"
        || (name.starts_with(".env.")
            && !matches!(name, ".env.example" | ".env.sample" | ".env.template"));
    env || matches!(name, ".npmrc" | ".netrc" | ".pypirc" | ".git-credentials")
        || Path::new(name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("pem") || ext.eq_ignore_ascii_case("key"))
        || ["id_rsa", "id_ecdsa", "id_ed25519"]
            .iter()
            .any(|key| name.starts_with(key))
}

/// Why a call returned nothing. The model reads the message.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum RepoError {
    /// The path leaves the context, is absolute, or is hidden.
    #[error("{0}: outside the repository or hidden")]
    Denied(String),
    /// Nothing at the path.
    #[error("{0}: not found")]
    NotFound(String),
    /// The file is not UTF-8 text.
    #[error("{0}: not a text file")]
    Binary(String),
    /// The pattern is empty.
    #[error("the pattern is empty")]
    EmptyPattern,
}

/// The repository, read-only.
#[derive(Debug, Clone)]
pub(crate) struct Repo {
    /// The context, canonical.
    root: PathBuf,
}

impl Repo {
    /// The repository at `root`.
    pub(crate) fn new(root: &Path) -> std::io::Result<Self> {
        Ok(Self {
            root: root.canonicalize()?,
        })
    }

    /// The path `path` names inside the context, or why it does not.
    fn resolve(&self, path: &str) -> Result<PathBuf, RepoError> {
        let relative = Path::new(path.trim_start_matches("./"));
        let mut joined = self.root.clone();
        for component in relative.components() {
            match component {
                Component::Normal(name) => {
                    let name = name.to_string_lossy();
                    if HIDDEN_DIRS.contains(&name.as_ref()) || secret_file(&name) {
                        return Err(RepoError::Denied(path.to_owned()));
                    }
                    joined.push(name.as_ref());
                }
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(RepoError::Denied(path.to_owned()));
                }
            }
        }
        let real = joined
            .canonicalize()
            .map_err(|_missing| RepoError::NotFound(path.to_owned()))?;
        if real.starts_with(&self.root) {
            Ok(real)
        } else {
            Err(RepoError::Denied(path.to_owned()))
        }
    }

    /// Whether a file exists at `path`, such as `Dockerfile`.
    pub(crate) fn file(&self, path: &str) -> Option<String> {
        let path = self.resolve(path).ok()?;
        fs::read_to_string(path).ok()
    }

    /// The entries under `path`, `depth` levels down, directories ending in `/`.
    pub(crate) fn list(&self, path: &str, depth: u32) -> Result<String, RepoError> {
        let dir = self.resolve(path)?;
        let mut out = Bounded::default();
        self.walk(
            &dir,
            depth.clamp(1, MAX_DEPTH),
            &mut |entry, is_dir, size| {
                let line = if is_dir {
                    format!("{entry}/\n")
                } else {
                    format!("{entry} ({size} B)\n")
                };
                out.push(&line)
            },
        );
        Ok(out.finish("list a subdirectory to see more"))
    }

    /// `path`'s text from byte `offset`.
    pub(crate) fn read(&self, path: &str, offset: usize) -> Result<String, RepoError> {
        let file = self.resolve(path)?;
        let bytes = fs::read(&file).map_err(|_unreadable| RepoError::NotFound(path.to_owned()))?;
        let text =
            String::from_utf8(bytes).map_err(|_not_utf8| RepoError::Binary(path.to_owned()))?;
        let start = floor_char_boundary(&text, offset);
        let end = floor_char_boundary(&text, start.saturating_add(MAX_RESULT));
        let chunk = text.get(start..end).unwrap_or_default();
        if end < text.len() {
            Ok(format!(
                "{chunk}\n[{} of {} bytes; read on with offset={end}]",
                end - start,
                text.len()
            ))
        } else {
            Ok(chunk.to_owned())
        }
    }

    /// The lines under `path` that contain `pattern`, as `file:line: text`.
    pub(crate) fn grep(&self, pattern: &str, path: &str) -> Result<String, RepoError> {
        if pattern.is_empty() {
            return Err(RepoError::EmptyPattern);
        }
        let dir = self.resolve(path)?;
        let mut out = Bounded::default();
        let mut search = |file: &Path, name: &str| {
            let Ok(text) = fs::read_to_string(file) else {
                return true;
            };
            for (number, line) in text.lines().enumerate() {
                if line.contains(pattern) && !out.push(&format!("{name}:{}: {line}\n", number + 1))
                {
                    return false;
                }
            }
            true
        };
        if dir.is_file() {
            search(&dir, path);
        } else {
            self.walk(&dir, u32::MAX, &mut |entry, is_dir, size| {
                is_dir || size > MAX_GREP_FILE || search(&self.root.join(entry), entry)
            });
        }
        Ok(out.finish("search a subdirectory to see more"))
    }

    /// Calls `visit` with each entry under `dir` (path relative to the root, whether it is a
    /// directory, size) in name order, `depth` levels down, until it returns `false`.
    fn walk(&self, dir: &Path, depth: u32, visit: &mut dyn FnMut(&str, bool, u64) -> bool) -> bool {
        let Ok(entries) = fs::read_dir(dir) else {
            return true;
        };
        let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            // Symbolic links are not followed, so a walk stays inside the context.
            if kind.is_symlink() {
                continue;
            }
            let is_dir = kind.is_dir();
            if (is_dir && HIDDEN_DIRS.contains(&name.as_str())) || secret_file(&name) {
                continue;
            }
            let path = entry.path();
            let relative = path.strip_prefix(&self.root).unwrap_or(&path);
            let relative = relative.to_string_lossy();
            let size = entry.metadata().map_or(0, |m| m.len());
            if !visit(&relative, is_dir, size) {
                return false;
            }
            if is_dir && depth > 1 && !self.walk(&path, depth - 1, visit) {
                return false;
            }
        }
        true
    }
}

/// Text that stops growing at [`MAX_RESULT`] bytes.
#[derive(Debug, Default)]
struct Bounded {
    text: String,
    full: bool,
}

impl Bounded {
    /// Adds `line`, or returns `false` once the result is full.
    fn push(&mut self, line: &str) -> bool {
        if self.text.len() + line.len() > MAX_RESULT {
            self.full = true;
            return false;
        }
        self.text.push_str(line);
        true
    }

    /// The text, with `more` when it was cut.
    fn finish(mut self, more: &str) -> String {
        if self.full {
            write!(self.text, "[cut at {MAX_RESULT} bytes; {more}]").unwrap_or_default();
        } else if self.text.is_empty() {
            self.text.push_str("(nothing)");
        }
        self.text
    }
}

/// The largest char boundary of `text` at or before `index`.
fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> (crate::testing::Dir, Repo) {
        let dir = crate::testing::Dir::new();
        dir.write(
            "package.json",
            r#"{"name":"app","scripts":{"start":"node index.js"}}"#,
        );
        dir.write("index.js", "const port = process.env.PORT;\n");
        dir.write("src/lib.js", "export const port = 3000;\n");
        dir.write(".env", "SECRET=1\n");
        dir.write(".env.example", "PORT=\n");
        dir.write("node_modules/x/index.js", "port\n");
        dir.write(".git/config", "[core]\n");
        dir.write("deploy.pem", "-----BEGIN-----\n");
        let repo = Repo::new(dir.path()).unwrap();
        (dir, repo)
    }

    #[test]
    fn lists_without_hidden_entries() {
        let (_dir, repo) = repo();
        let list = repo.list(".", 2).unwrap();
        assert!(list.contains("src/\n"));
        assert!(list.contains("src/lib.js"));
        assert!(list.contains(".env.example"));
        for hidden in ["node_modules", ".git", "deploy.pem", ".env (", ".env\n"] {
            assert!(!list.contains(hidden), "{hidden} listed:\n{list}");
        }
    }

    #[test]
    fn refuses_paths_outside_or_hidden() {
        let (_dir, repo) = repo();
        for path in [
            "../etc/passwd",
            "/etc/passwd",
            ".env",
            "node_modules/x/index.js",
            ".git/config",
        ] {
            assert_eq!(
                repo.read(path, 0),
                Err(RepoError::Denied(path.to_owned())),
                "{path}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_link_out_of_the_context() {
        let (dir, repo) = repo();
        std::os::unix::fs::symlink("/etc", dir.path().join("etc")).unwrap();
        assert_eq!(
            repo.read("etc/hosts", 0),
            Err(RepoError::Denied("etc/hosts".to_owned()))
        );
        assert!(!repo.list(".", 1).unwrap().contains("etc"));
    }

    #[test]
    fn reads_long_files_in_pages() {
        let (dir, repo) = repo();
        let long = "é".repeat(MAX_RESULT);
        dir.write("long.txt", &long);
        let first = repo.read("long.txt", 0).unwrap();
        let (chunk, rest) = first.rsplit_once('\n').unwrap();
        assert!(chunk.len() <= MAX_RESULT);
        let offset: usize = rest
            .trim_end_matches(']')
            .rsplit_once("offset=")
            .unwrap()
            .1
            .parse()
            .unwrap();
        assert_eq!(offset, chunk.len());
        let second = repo.read("long.txt", offset).unwrap();
        assert_eq!(format!("{chunk}{second}"), long);
    }

    #[test]
    fn greps_visible_files_only() {
        let (_dir, repo) = repo();
        let found = repo.grep("port", ".").unwrap();
        assert!(found.contains("index.js:1: const port"));
        assert!(found.contains("src/lib.js:1:"));
        assert!(!found.contains("node_modules"));
        assert_eq!(repo.grep("", "."), Err(RepoError::EmptyPattern));
    }

    #[test]
    fn cuts_results_at_the_limit() {
        let (dir, repo) = repo();
        dir.write("many.txt", &"port\n".repeat(MAX_RESULT));
        let found = repo.grep("port", "many.txt").unwrap();
        assert!(found.len() <= MAX_RESULT + 100);
        assert!(found.ends_with("search a subdirectory to see more]"));
    }

    #[test]
    fn build_ignore_covers_every_hidden_file() {
        for name in [
            ".env",
            ".env.local",
            ".npmrc",
            ".netrc",
            "deploy.pem",
            "tls.key",
            "id_rsa",
        ] {
            assert!(secret_file(name), "{name}");
        }
        assert!(!secret_file(".env.example"));
        assert!(!secret_file("package.json"));
    }
}
