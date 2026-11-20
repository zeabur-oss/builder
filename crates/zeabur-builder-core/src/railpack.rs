//! Railpack images that run as a non-root user.
//!
//! Railpack's frontend writes an image config without `User` ([`convert.go`]), so its images run
//! as root, and Kubernetes refuses to start them under `runAsNonRoot`. Its deploy copies the paths
//! each layer includes out of the step that made them, and BuildKit's copy keeps the owner those
//! files had there when the copy names none ([`backend_unix.go`]): root.
//!
//! Two changes make the image run as [`IMAGE_UID`]:
//!
//! - [`own_deploy_layers`] ends each step the deploy copies from with [`OWN_COMMAND`], so the
//!   copies already belong to the user and the image carries no second copy of re-owned files.
//! - [`dockerfile`] is a second build `FROM` the Railpack image. It hands over what is still root's
//!   under `/app` and `/root` (the directories the copies created, the base image's `/root`), adds
//!   the user to `/etc/passwd` and `/etc/group`, and sets `USER`. Its one layer holds directory
//!   entries and the two files.
//!
//! `HOME` stays `/root`: Railpack's layers put mise's state and the build's `~/.cache` there.
//!
//! [`convert.go`]: https://github.com/railwayapp/railpack/blob/v0.43.0/buildkit/convert.go
//! [`backend_unix.go`]: https://github.com/moby/buildkit/blob/v0.33.1/solver/llbsolver/file/backend_unix.go

use serde::{Deserialize, Serialize};

/// Fields of a Railpack plan that this code does not read, written back as they are.
#[expect(
    clippy::disallowed_types,
    reason = "Railpack's plan has fields of any shape"
)]
type Unread = serde_json::Map<String, serde_json::Value>;

/// A Railpack build plan ([`plan.go`]), with the fields this crate reads. The rest stays as it is.
///
/// [`plan.go`]: https://github.com/railwayapp/railpack/blob/v0.43.0/core/plan/plan.go
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Plan {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    steps: Vec<Step>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deploy: Option<Deploy>,
    #[serde(flatten)]
    rest: Unread,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
struct Step {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    commands: Option<Vec<Command>>,
    #[serde(flatten)]
    rest: Unread,
}

/// A step's command: one that runs a program, or another kind this code does not read.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
enum Command {
    Exec(Exec),
    Other(Unread),
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Exec {
    cmd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    custom_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
struct Deploy {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    inputs: Vec<Layer>,
    #[serde(flatten)]
    rest: Unread,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
struct Layer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    step: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    local: bool,
    #[serde(flatten)]
    rest: Unread,
}

/// The user and group a Railpack image runs as.
pub const IMAGE_UID: u32 = 1000;

/// The directories the user owns: the app, and `HOME`.
const OWNED: &str = "/app /root";

/// Gives `/app` and `/root` to [`IMAGE_UID`] at the end of a step. `-xdev` leaves the step's cache
/// mounts alone; a step image without `find` keeps root's files, and the [`dockerfile`] build hands
/// them over instead.
pub const OWN_COMMAND: &str = "sh -c 'command -v find >/dev/null || exit 0; for d in /app /root; do if [ -e \"$d\" ]; then find \"$d\" -xdev -exec chown -h 1000:1000 {} +; fi; done'";

const OWN_NAME: &str = "give /app and /root to uid 1000";

/// Appends [`OWN_COMMAND`] to every step a deploy layer copies from, once; returns their names.
pub fn own_deploy_layers(plan: &mut Plan) -> Vec<String> {
    let mut names: Vec<&str> = Vec::new();
    for layer in plan.deploy.iter().flat_map(|d| &d.inputs) {
        if let Some(step) = layer.step.as_deref()
            && !layer.local
            && !names.contains(&step)
        {
            names.push(step);
        }
    }
    let names: Vec<String> = names.into_iter().map(str::to_owned).collect();
    let own = Command::Exec(Exec {
        cmd: OWN_COMMAND.to_owned(),
        custom_name: Some(OWN_NAME.to_owned()),
    });
    let mut owned = Vec::new();
    for step in &mut plan.steps {
        if !names.contains(&step.name) {
            continue;
        }
        let commands = step.commands.get_or_insert_default();
        if commands.last() != Some(&own) {
            commands.push(own.clone());
        }
        owned.push(step.name.clone());
    }
    owned
}

/// The second build: `FROM app`, the Railpack image passed as the named context `app`.
#[must_use]
pub fn dockerfile() -> String {
    let uid = IMAGE_UID;
    format!(
        r#"FROM app
RUN set -eu; \
    for dir in {OWNED}; do \
      if [ -e "$dir" ]; then find "$dir" -xdev -user 0 -exec chown -h {uid}:{uid} {{}} +; fi; \
    done; \
    grep -q '^[^:]*:[^:]*:{uid}:' /etc/passwd || echo 'app:x:{uid}:{uid}::/root:/bin/sh' >> /etc/passwd; \
    grep -q '^[^:]*:[^:]*:{uid}:' /etc/group || echo 'app:x:{uid}:' >> /etc/group
ENV HOME=/root
USER {uid}:{uid}
"#
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn plan() -> Plan {
        serde_json::from_value(json!({
            "steps": [
                {"name": "packages:mise", "inputs": [{"image": "railpack-builder"}],
                 "commands": [{"cmd": "mise install"}]},
                {"name": "install", "inputs": [{"step": "packages:mise"}],
                 "commands": [{"cmd": "npm ci"}]},
                {"name": "build", "inputs": [{"step": "install"}, {"local": true, "include": ["."]}],
                 "commands": [{"cmd": "npm run build"}]},
                {"name": "packages:apt:runtime", "inputs": [{"image": "railpack-runtime"}]}
            ],
            "deploy": {
                "base": {"step": "packages:apt:runtime"},
                "inputs": [
                    {"step": "packages:mise", "include": ["/mise/installs", "/root/.local/state/mise"]},
                    {"step": "build", "include": ["/app/node_modules"]},
                    {"step": "build", "include": [".", "/root/.cache"]},
                    {"local": true, "step": "install", "include": ["Caddyfile"]},
                    {"image": "caddy", "include": ["/usr/bin/caddy"]}
                ]
            }
        }))
        .unwrap()
    }

    fn commands(plan: &Plan, step: &str) -> Vec<String> {
        let step = plan.steps.iter().find(|s| s.name == step).unwrap();
        step.commands
            .iter()
            .flatten()
            .map(|c| match c {
                Command::Exec(exec) => exec.cmd.clone(),
                Command::Other(other) => panic!("not a program: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn steps_the_deploy_copies_from_end_by_giving_the_app_to_the_user() {
        let mut plan = plan();
        assert_eq!(own_deploy_layers(&mut plan), ["packages:mise", "build"]);
        assert_eq!(
            commands(&plan, "packages:mise"),
            ["mise install", OWN_COMMAND]
        );
        assert_eq!(commands(&plan, "build"), ["npm run build", OWN_COMMAND]);
        assert_eq!(commands(&plan, "install"), ["npm ci"]);
        assert_eq!(
            commands(&plan, "packages:apt:runtime"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn owning_twice_appends_once() {
        let mut plan = plan();
        own_deploy_layers(&mut plan);
        let once = plan.clone();
        own_deploy_layers(&mut plan);
        assert_eq!(plan, once);
    }

    #[test]
    fn a_plan_without_deploy_inputs_is_left_alone() {
        for json in [json!({}), json!({"deploy": {"startCmd": "x"}})] {
            let mut plan: Plan = serde_json::from_value(json.clone()).unwrap();
            assert_eq!(own_deploy_layers(&mut plan), Vec::<String>::new());
            assert_eq!(serde_json::to_value(&plan).unwrap(), json);
        }
        let mut no_commands: Plan = serde_json::from_value(json!({
            "steps": [{"name": "build"}],
            "deploy": {"inputs": [{"step": "build", "include": ["."]}]}
        }))
        .unwrap();
        assert_eq!(own_deploy_layers(&mut no_commands), ["build"]);
        assert_eq!(commands(&no_commands, "build"), [OWN_COMMAND]);
    }

    #[test]
    fn fields_the_plan_does_not_read_are_written_back() {
        let json = json!({
            "steps": [{"name": "build", "commands": [{"path": "/x"}], "inputs": [{"image": "b"}]}],
            "deploy": {"inputs": [{"local": true, "include": ["."]}], "startCmd": "node ."},
            "caches": {"npm": {"directory": "/root/.npm", "type": "shared"}},
            "secrets": ["NPM_TOKEN"],
        });
        let mut plan: Plan = serde_json::from_value(json.clone()).unwrap();
        own_deploy_layers(&mut plan);
        assert_eq!(serde_json::to_value(&plan).unwrap(), json);
    }

    #[test]
    fn the_command_survives_railpacks_word_splitting() {
        // Railpack runs a step's command through `llb.Shlex`: one `sh -c` with one script.
        let script = OWN_COMMAND
            .strip_prefix("sh -c '")
            .and_then(|s| s.strip_suffix('\''))
            .unwrap();
        assert!(!script.contains('\''));
        assert!(script.contains(&format!("for d in {OWNED};")));
        assert!(script.contains(&format!("chown -h {IMAGE_UID}:{IMAGE_UID}")));
    }

    #[test]
    fn the_image_runs_as_a_numeric_user() {
        let dockerfile = dockerfile();
        assert!(dockerfile.starts_with("FROM app\n"));
        assert!(dockerfile.ends_with("ENV HOME=/root\nUSER 1000:1000\n"));
        assert!(dockerfile.contains("-user 0 -exec chown -h 1000:1000 {} +"));
        assert!(dockerfile.contains("echo 'app:x:1000:1000::/root:/bin/sh' >> /etc/passwd"));
    }
}
