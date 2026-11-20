//! Image tags.
//!
//! The default tag `{{.Branch}}-{{.UnixTime}}-{{.ShortSHA}}` is immutable (one per build) and sorts
//! by time, which is what Flux image automation's `numerical` policy, Keel and Argo CD Image
//! Updater need to pick the newest build without trusting `latest`.
//!
//! A tag must match `[A-Za-z0-9_][A-Za-z0-9._-]{0,127}` ([OCI distribution], "Pulling
//! manifests"). Branch names allow far more (`/`, `+`, Unicode), so the branch is sanitised and,
//! when the tag would be too long, shortened; the time and commit parts are never cut, since they
//! are what makes the tag unique and sortable.
//!
//! `.UnixTime` is the `BuildRun`'s creation time, passed in by the caller, so a retried run of the
//! same `BuildRun` pushes the same tag.
//!
//! The template language is the subset of Go's `text/template` that the plan's API exposes:
//! `{{.Name}}` fields, with optional spaces inside the braces. Anything else is rejected at
//! render time rather than passed through.
//!
//! [OCI distribution]: https://github.com/opencontainers/distribution-spec/blob/main/spec.md#pulling-manifests

use thiserror::Error;

/// The default tag template.
pub const DEFAULT_TEMPLATE: &str = "{{.Branch}}-{{.UnixTime}}-{{.ShortSHA}}";

/// The longest tag the registry accepts.
pub const MAX_TAG_LEN: usize = 128;

/// Values a tag template can use.
#[derive(Debug, Clone, Copy)]
pub struct TagInputs<'a> {
    /// The ref the build follows, such as `main` or `feature/login`.
    pub branch: &'a str,
    /// The full commit SHA.
    pub sha: &'a str,
    /// The `BuildRun`'s creation time, in Unix seconds.
    pub unix_time: i64,
    /// The `BuildRun`'s name.
    pub build_run: &'a str,
}

/// Why a template cannot be rendered.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TagError {
    /// `{{` without `}}`, or an action other than a field.
    #[error("unsupported template syntax at `{0}`: only {{{{.Field}}}} is allowed")]
    Syntax(String),
    /// A field the template language does not have.
    #[error("unknown template field `.{0}`: use .Branch, .ShortSHA, .SHA, .UnixTime or .BuildRun")]
    Field(String),
    /// The fixed parts alone are longer than a tag may be.
    #[error("the tag would be longer than {MAX_TAG_LEN} characters even with the branch shortened")]
    TooLong,
}

/// Renders a tag.
///
/// # Errors
///
/// When the template has unsupported syntax or fields, or cannot fit in [`MAX_TAG_LEN`].
pub fn render(template: &str, inputs: &TagInputs<'_>) -> Result<String, TagError> {
    let parts = parse(template)?;
    let short_sha: String = inputs.sha.chars().take(7).collect();
    let unix_time = inputs.unix_time.to_string();
    let branch = sanitise(inputs.branch);
    let fixed_len: usize = parts
        .iter()
        .map(|part| match part {
            Part::Text(text) => sanitise(text).len(),
            Part::Field(Field::Branch) => 0,
            Part::Field(Field::ShortSha) => short_sha.len(),
            Part::Field(Field::Sha) => inputs.sha.len(),
            Part::Field(Field::UnixTime) => unix_time.len(),
            Part::Field(Field::BuildRun) => sanitise(inputs.build_run).len(),
        })
        .sum();
    let branch_uses = parts
        .iter()
        .filter(|p| matches!(p, Part::Field(Field::Branch)))
        .count();
    let room = MAX_TAG_LEN
        .checked_sub(fixed_len)
        .ok_or(TagError::TooLong)?;
    let branch_budget = room.checked_div(branch_uses).unwrap_or(0);
    if branch_uses > 0 && branch_budget == 0 && !branch.is_empty() {
        return Err(TagError::TooLong);
    }
    let branch: String = branch.chars().take(branch_budget).collect();
    let branch = branch.trim_end_matches(['.', '-']);

    let mut tag: String = parts
        .iter()
        .map(|part| match part {
            Part::Text(text) => sanitise(text),
            Part::Field(Field::Branch) => branch.to_owned(),
            Part::Field(Field::ShortSha) => short_sha.clone(),
            Part::Field(Field::Sha) => inputs.sha.to_owned(),
            Part::Field(Field::UnixTime) => unix_time.clone(),
            Part::Field(Field::BuildRun) => sanitise(inputs.build_run),
        })
        .collect();
    if tag.starts_with(['.', '-']) {
        tag.insert(0, '_');
    }
    if tag.is_empty() || tag.len() > MAX_TAG_LEN {
        return Err(TagError::TooLong);
    }
    Ok(tag)
}

/// Whether a string is a valid tag.
#[must_use]
pub fn is_valid(tag: &str) -> bool {
    let mut bytes = tag.bytes();
    tag.len() <= MAX_TAG_LEN
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Branch,
    ShortSha,
    Sha,
    UnixTime,
    BuildRun,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Text(String),
    Field(Field),
}

fn parse(template: &str) -> Result<Vec<Part>, TagError> {
    let mut parts = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        let (text, after) = rest.split_at(open);
        if !text.is_empty() {
            parts.push(Part::Text(text.to_owned()));
        }
        let after = after.trim_start_matches("{{");
        let close = after
            .find("}}")
            .ok_or_else(|| TagError::Syntax(rest.to_owned()))?;
        let (action, after) = after.split_at(close);
        let name = action
            .trim()
            .strip_prefix('.')
            .ok_or_else(|| TagError::Syntax(action.to_owned()))?;
        let field = match name {
            "Branch" => Field::Branch,
            "ShortSHA" => Field::ShortSha,
            "SHA" => Field::Sha,
            "UnixTime" => Field::UnixTime,
            "BuildRun" => Field::BuildRun,
            other if other.bytes().all(|b| b.is_ascii_alphanumeric()) => {
                return Err(TagError::Field(other.to_owned()));
            }
            _ => return Err(TagError::Syntax(action.to_owned())),
        };
        parts.push(Part::Field(field));
        rest = after.strip_prefix("}}").unwrap_or(after);
    }
    if rest.contains("}}") {
        return Err(TagError::Syntax(rest.to_owned()));
    }
    if !rest.is_empty() {
        parts.push(Part::Text(rest.to_owned()));
    }
    Ok(parts)
}

/// Replaces every character a tag cannot hold with `-`, so `feature/login` becomes `feature-login`.
fn sanitise(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "3e1a9c2d4f0b5a6c7d8e9f0a1b2c3d4e5f6a7b8c";

    fn inputs(branch: &str) -> TagInputs<'_> {
        TagInputs {
            branch,
            sha: SHA,
            unix_time: 1_761_058_800,
            build_run: "api-x7k2p",
        }
    }

    #[test]
    fn default_template() {
        assert_eq!(
            render(DEFAULT_TEMPLATE, &inputs("main")).unwrap(),
            "main-1761058800-3e1a9c2"
        );
        assert_eq!(
            render(DEFAULT_TEMPLATE, &inputs("feature/login")).unwrap(),
            "feature-login-1761058800-3e1a9c2"
        );
        assert_eq!(
            render(DEFAULT_TEMPLATE, &inputs("release+1.0")).unwrap(),
            "release-1.0-1761058800-3e1a9c2"
        );
    }

    #[test]
    fn spaces_and_other_fields() {
        let tag = render("{{ .BuildRun }}_{{.SHA}}", &inputs("main")).unwrap();
        assert_eq!(tag, format!("api-x7k2p_{SHA}"));
    }

    #[test]
    fn long_branches_are_shortened_not_the_suffix() {
        let branch = "a".repeat(300);
        let tag = render(DEFAULT_TEMPLATE, &inputs(&branch)).unwrap();
        assert_eq!(tag.len(), MAX_TAG_LEN);
        assert!(tag.ends_with("-1761058800-3e1a9c2"));
        assert!(is_valid(&tag));
    }

    #[test]
    fn leading_separator_is_prefixed() {
        let tag = render(DEFAULT_TEMPLATE, &inputs("-x")).unwrap();
        assert!(is_valid(&tag), "{tag}");
        assert!(is_valid(
            &render(DEFAULT_TEMPLATE, &inputs(".hidden")).unwrap()
        ));
    }

    #[test]
    fn rejects_unknown_syntax() {
        assert_eq!(
            render("{{.Commit}}", &inputs("main")),
            Err(TagError::Field("Commit".to_owned()))
        );
        assert!(matches!(
            render("{{if .Branch}}x{{end}}", &inputs("main")),
            Err(TagError::Syntax(_))
        ));
        assert!(matches!(
            render("{{.Branch", &inputs("main")),
            Err(TagError::Syntax(_))
        ));
        assert!(matches!(
            render("x}}", &inputs("main")),
            Err(TagError::Syntax(_))
        ));
        assert_eq!(
            render(
                &format!("{}{{{{.Branch}}}}", "x".repeat(130)),
                &inputs("main")
            ),
            Err(TagError::TooLong)
        );
    }

    #[test]
    fn validity() {
        assert!(is_valid("main-1-abc"));
        assert!(is_valid("_x"));
        assert!(!is_valid("-x"));
        assert!(!is_valid("a/b"));
        assert!(!is_valid(""));
        assert!(!is_valid(&"a".repeat(129)));
    }
}
