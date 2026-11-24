//! The read-only view of the repository the agent's `list`, `read` and `grep` tools give.
//!
//! Every path is relative to the context and resolves inside it, symbolic links included. Hidden
//! entries (version control, dependency directories, credentials) are neither listed, read nor
//! searched, after symbolic links resolve too, and [`dockerignore`] keeps the credentials out of
//! every build context. Each result
//! is at most [`MAX_RESULT`] bytes and says how to read on.

use std::fmt::Write as _;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

/// The largest result of one call.
pub(crate) const MAX_RESULT: usize = 12 * 1024;
/// Files larger than this are not searched.
const MAX_GREP_FILE: u64 = 1 << 20;
/// The deepest `list` goes.
const MAX_DEPTH: u32 = 4;

/// Directories no tool shows: other version control, installed dependencies, caches.
const HIDDEN_DIRS: &[&str] = &[
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

/// Names, at any depth, that every build leaves out and no tool shows. As in `.dockerignore`, the
/// last entry that matches a name decides.
const SECRETS: &[Secret] = &[
    Secret::Prefix(".env."),
    Secret::Kept(".env.example"),
    Secret::Kept(".env.sample"),
    Secret::Kept(".env.template"),
    Secret::Name(".git"),
    Secret::Name(".env"),
    Secret::Name(".npmrc"),
    Secret::Name(".netrc"),
    Secret::Name(".pypirc"),
    Secret::Name(".git-credentials"),
    Secret::Extension("pem"),
    Secret::Extension("key"),
    Secret::Prefix("id_rsa"),
    Secret::Prefix("id_ecdsa"),
    Secret::Prefix("id_ed25519"),
];

/// An entry of [`SECRETS`].
#[derive(Debug, Clone, Copy)]
enum Secret {
    /// This name, such as `.npmrc`.
    Name(&'static str),
    /// Names that start with this, such as `id_rsa.pub`.
    Prefix(&'static str),
    /// Names that end in a dot and this extension, in any case, such as `tls.KEY`.
    Extension(&'static str),
    /// This name, which an earlier entry matches but which holds no credentials, such as
    /// `.env.example`. A later entry still matches the entries inside a directory of this name.
    Kept(&'static str),
}

impl Secret {
    /// Whether an entry named `name` matches.
    fn matches(self, name: &str) -> bool {
        match self {
            Self::Name(secret) | Self::Kept(secret) => name == secret,
            Self::Prefix(prefix) => name.starts_with(prefix),
            Self::Extension(extension) => name
                .rsplit_once('.')
                .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case(extension)),
        }
    }

    /// Whether the names that this entry matches are left out.
    const fn hides(self) -> bool {
        match self {
            Self::Name(_) | Self::Prefix(_) | Self::Extension(_) => true,
            Self::Kept(_) => false,
        }
    }

    /// The `.dockerignore` rule for the names [`Self::matches`] accepts.
    fn rule(self) -> String {
        match self {
            Self::Name(name) => format!("**/{name}"),
            Self::Kept(name) => format!("!**/{name}"),
            Self::Prefix(prefix) => format!("**/{prefix}*"),
            Self::Extension(extension) => {
                let mut any_case = String::new();
                for c in extension.chars() {
                    any_case.extend(['[', c.to_ascii_lowercase(), c.to_ascii_uppercase(), ']']);
                }
                format!("**/*.{any_case}")
            }
        }
    }
}

/// `.dockerignore` rules every build ends with, so a `COPY . .` never takes credentials.
static BUILD_IGNORE: LazyLock<String> =
    LazyLock::new(|| SECRETS.iter().map(|secret| secret.rule() + "\n").collect());

/// The `.dockerignore` of a build with `rules`. The last matching rule wins, so [`BUILD_IGNORE`]
/// goes last and a rule such as `!.env` cannot take credentials back in.
pub(crate) fn dockerignore(rules: &str) -> String {
    format!("{}\n{}", rules.trim(), *BUILD_IGNORE)
}

/// The rules of a `.dockerignore` that [`dockerignore`] wrote.
pub(crate) fn rules_of(dockerignore: &str) -> &str {
    dockerignore
        .strip_suffix(BUILD_IGNORE.as_str())
        .unwrap_or(dockerignore)
        .trim()
}

/// Whether no tool shows an entry named `name`.
fn hidden(name: &str) -> bool {
    HIDDEN_DIRS.contains(&name) || secret_file(name)
}

/// Whether [`BUILD_IGNORE`] leaves out an entry named `name`.
fn secret_file(name: &str) -> bool {
    SECRETS
        .iter()
        .rfind(|secret| secret.matches(name))
        .is_some_and(|secret| secret.hides())
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
                    if hidden(&name) {
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
        // A link such as `config.txt -> .env` passes the checks above, so check where it leads.
        let inside = real.strip_prefix(&self.root).is_ok_and(|relative| {
            relative
                .components()
                .all(|component| !hidden(&component.as_os_str().to_string_lossy()))
        });
        if inside {
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
            &serde_json::json!({"name": "app", "scripts": {"start": "node index.js"}}).to_string(),
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

    #[cfg(unix)]
    #[test]
    fn refuses_a_link_to_a_hidden_file() {
        let (dir, repo) = repo();
        std::os::unix::fs::symlink(".env", dir.path().join("config.txt")).unwrap();
        std::os::unix::fs::symlink(".git", dir.path().join("history")).unwrap();
        for path in ["config.txt", "history/config"] {
            assert_eq!(
                repo.read(path, 0),
                Err(RepoError::Denied(path.to_owned())),
                "{path}"
            );
        }
        assert_eq!(
            repo.grep("SECRET", "config.txt"),
            Err(RepoError::Denied("config.txt".to_owned()))
        );
    }

    #[test]
    fn credentials_stay_ignored_after_the_model_s_rules() {
        let file = dockerignore("dist\n!**\n!.env\n");
        assert!(file.starts_with("dist\n!**\n!.env\n"));
        assert!(file.ends_with(BUILD_IGNORE.as_str()));
        assert_eq!(rules_of(&file), "dist\n!**\n!.env");
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

    /// A name `pattern` matches, with each `[xX]` class read as its `pick`th character.
    fn example(pattern: &str, pick: usize) -> String {
        let mut name = String::new();
        let mut chars = pattern.chars();
        while let Some(c) = chars.next() {
            match c {
                '*' => name.push('x'),
                '[' => {
                    let class: Vec<char> = chars.by_ref().take_while(|&c| c != ']').collect();
                    name.push(class[pick % class.len()]);
                }
                c => name.push(c),
            }
        }
        name
    }

    /// Whether the glob `pattern`, with `*` and `[...]`, matches all of `name`.
    fn glob(pattern: &[char], name: &[char]) -> bool {
        match pattern.first() {
            None => name.is_empty(),
            Some('*') => (0..=name.len()).any(|i| glob(&pattern[1..], &name[i..])),
            Some('[') => {
                let end = pattern.iter().position(|&c| c == ']').unwrap();
                name.first().is_some_and(|c| pattern[1..end].contains(c))
                    && glob(&pattern[end + 1..], &name[1..])
            }
            Some(c) => name.first() == Some(c) && glob(&pattern[1..], &name[1..]),
        }
    }

    /// Whether Docker leaves out `path` with [`BUILD_IGNORE`]. A rule `**/<glob>` matches a path
    /// if it matches the path or a parent directory, and the last matching rule decides
    /// ([patternmatcher]).
    ///
    /// [patternmatcher]: https://github.com/moby/patternmatcher/blob/main/patternmatcher.go
    fn docker_ignores(path: &str) -> bool {
        let mut ignored = false;
        for rule in BUILD_IGNORE.lines() {
            let (keep, rule) = rule.strip_prefix('!').map_or((false, rule), |r| (true, r));
            let pattern: Vec<char> = rule.strip_prefix("**/").unwrap().chars().collect();
            let matched = path
                .split('/')
                .any(|component| glob(&pattern, &component.chars().collect::<Vec<_>>()));
            if matched {
                ignored = !keep;
            }
        }
        ignored
    }

    #[test]
    fn tools_refuse_what_every_build_leaves_out() {
        let (dir, repo) = repo();
        let mut refused = vec![
            "id_rsa".to_owned(),
            "id_ecdsa.pub".to_owned(),
            "deploy/id_ed25519".to_owned(),
            ".env.example/tls.key".to_owned(),
            ".env.template/.env".to_owned(),
        ];
        let mut shown = Vec::new();
        for rule in BUILD_IGNORE.lines() {
            let kept = rule.strip_prefix('!');
            let list = if kept.is_some() {
                &mut shown
            } else {
                &mut refused
            };
            let pattern = kept.unwrap_or(rule);
            for pick in [0, 1] {
                list.push(example(&pattern.replace("**/", "app/"), pick));
            }
        }
        for path in &refused {
            assert!(
                docker_ignores(path),
                "{path} not ignored:\n{}",
                *BUILD_IGNORE
            );
            dir.write(path, "secret\n");
            assert_eq!(
                repo.read(path, 0),
                Err(RepoError::Denied(path.clone())),
                "{path}"
            );
        }
        for path in &shown {
            assert!(!docker_ignores(path), "{path} ignored:\n{}", *BUILD_IGNORE);
            dir.write(path, "PORT=\n");
            assert_eq!(repo.read(path, 0).as_deref(), Ok("PORT=\n"), "{path}");
        }
        let list = repo.list(".", MAX_DEPTH).unwrap();
        for path in &refused {
            assert!(
                !list.contains(&format!("{path} (")),
                "{path} listed:\n{list}"
            );
        }
    }
}
