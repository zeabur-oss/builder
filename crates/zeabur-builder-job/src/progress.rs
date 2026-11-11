//! Turns `buildctl --progress rawjson` into NDJSON events for readers of the pod log.
//!
//! Each input line is one [`SolveStatus`]. A vertex is numbered in the order it first appears,
//! and the numbers go on across `buildctl` runs, so a number is unique within a build. Every event
//! carries the vertex's `name`. `stream` is `stdout` or `stderr`. Lines that are not JSON are
//! buildctl's own messages and become `message` events.

use std::collections::BTreeMap;

use base64::Engine as _;
use jiff::Timestamp;
use serde::Serialize;
use zeabur_builder_core::progress::SolveStatus;

#[derive(Debug, Default)]
struct Step {
    number: usize,
    name: String,
    done: bool,
    start: Option<Timestamp>,
    started_printed: bool,
}

#[derive(Debug, Default, Serialize)]
struct Record<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    vertex: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    event: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    time: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<&'a str>,
}

impl Record<'_> {
    fn json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// Renders the stream and remembers the last error, for the termination message.
#[derive(Debug, Default)]
pub(crate) struct Renderer {
    steps: BTreeMap<String, Step>,
    /// Vertices numbered so far, over all runs.
    numbered: usize,
    /// The last failed step's error, or buildctl's last `error:` line.
    pub(crate) last_error: Option<String>,
}

impl Renderer {
    /// Starts the next `buildctl` run: its vertices are numbered on from the last run's.
    pub(crate) fn next_run(&mut self) {
        self.steps.clear();
    }

    /// The events to print for one line of the stream, one JSON object each.
    pub(crate) fn line(&mut self, line: &str) -> Vec<String> {
        let Some(status) = SolveStatus::parse(line) else {
            if let Some(error) = line.strip_prefix("error: ") {
                self.last_error = Some(error.to_owned());
            }
            let event = Record {
                event: "message",
                text: Some(line),
                ..Record::default()
            };
            return vec![event.json()];
        };
        let mut out = Vec::new();
        for vertex in status.vertexes {
            let step = step(&mut self.steps, &mut self.numbered, &vertex.digest);
            if step.done {
                continue;
            }
            if !vertex.name.is_empty() {
                step.name = vertex.name;
            }
            if step.start.is_none() {
                step.start = vertex.started.as_deref().and_then(|t| t.parse().ok());
            }
            let base = Record {
                vertex: Some(step.number),
                name: Some(&step.name),
                ..Record::default()
            };
            if !step.started_printed
                && let Some(time) = &vertex.started
            {
                step.started_printed = true;
                out.push(
                    Record {
                        event: "started",
                        time: Some(time),
                        ..base
                    }
                    .json(),
                );
            }
            let seconds = elapsed(step.start, vertex.completed.as_deref());
            if let Some(error) = &vertex.error {
                out.push(
                    Record {
                        event: "error",
                        seconds,
                        message: Some(error),
                        ..base
                    }
                    .json(),
                );
                self.last_error = Some(format!("{}: {error}", step.name));
                step.done = true;
            } else if vertex.cached {
                out.push(
                    Record {
                        event: "cached",
                        ..base
                    }
                    .json(),
                );
                step.done = true;
            } else if vertex.completed.is_some() {
                out.push(
                    Record {
                        event: "done",
                        seconds,
                        ..base
                    }
                    .json(),
                );
                step.done = true;
            }
        }
        for log in status.logs {
            let Ok(data) = base64::engine::general_purpose::STANDARD.decode(&log.data) else {
                continue;
            };
            let step = step(&mut self.steps, &mut self.numbered, &log.vertex);
            let stream = if log.stream == 2 { "stderr" } else { "stdout" };
            for text in String::from_utf8_lossy(&data).lines() {
                let event = Record {
                    vertex: Some(step.number),
                    name: Some(&step.name),
                    event: "log",
                    stream: Some(stream),
                    text: Some(text),
                    ..Record::default()
                };
                out.push(event.json());
            }
        }
        out
    }
}

/// The step of a vertex, numbered on its first appearance.
fn step<'a>(
    steps: &'a mut BTreeMap<String, Step>,
    numbered: &mut usize,
    digest: &str,
) -> &'a mut Step {
    steps.entry(digest.to_owned()).or_insert_with(|| {
        *numbered += 1;
        Step {
            number: *numbered,
            ..Step::default()
        }
    })
}

/// Seconds between a step's start and its completion, when both are known.
fn elapsed(start: Option<Timestamp>, completed: Option<&str>) -> Option<f64> {
    let completed: Timestamp = completed?.parse().ok()?;
    Some(completed.duration_since(start?).as_secs_f64())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn render(renderer: &mut Renderer, stream: &str) -> Vec<Value> {
        let mut out = Vec::new();
        for line in stream.lines() {
            for event in renderer.line(line) {
                out.push(serde_json::from_str(&event).unwrap());
            }
        }
        out
    }

    #[test]
    fn renders_cached_failed_and_stderr_steps() {
        let stream = r#"{"vertexes":[{"digest":"sha256:a","name":"[1/3] FROM alpine","started":"2026-10-22T00:00:00Z"}]}
{"vertexes":[{"digest":"sha256:a","name":"[1/3] FROM alpine","cached":true}]}
{"vertexes":[{"digest":"sha256:b","name":"[2/3] RUN make","started":"2026-10-22T00:00:01Z"}]}
{"logs":[{"vertex":"sha256:b","stream":1,"data":"Y2MgLW8gYXBwCg=="},{"vertex":"sha256:b","stream":2,"data":"d2FybmluZwo="}]}
{"vertexes":[{"digest":"sha256:c","name":"[3/3] RUN sleep","started":"2026-10-22T00:00:03Z"}]}
{"vertexes":[{"digest":"sha256:c","name":"[3/3] RUN sleep","started":"2026-10-22T00:00:03Z","completed":"2026-10-22T00:00:05Z"}]}
{"vertexes":[{"digest":"sha256:b","name":"[2/3] RUN make","started":"2026-10-22T00:00:01Z","completed":"2026-10-22T00:00:03Z","error":"exit code: 2"}]}
{"vertexes":[{"digest":"sha256:b","name":"[2/3] RUN make","completed":"2026-10-22T00:00:09Z"}]}
error: failed to solve: exit code: 2"#;
        let mut renderer = Renderer::default();
        let out = render(&mut renderer, stream);
        let make = "[2/3] RUN make";
        assert_eq!(
            out,
            [
                json!({"vertex":1,"name":"[1/3] FROM alpine","event":"started","time":"2026-10-22T00:00:00Z"}),
                json!({"vertex":1,"name":"[1/3] FROM alpine","event":"cached"}),
                json!({"vertex":2,"name":make,"event":"started","time":"2026-10-22T00:00:01Z"}),
                json!({"vertex":2,"name":make,"event":"log","stream":"stdout","text":"cc -o app"}),
                json!({"vertex":2,"name":make,"event":"log","stream":"stderr","text":"warning"}),
                json!({"vertex":3,"name":"[3/3] RUN sleep","event":"started","time":"2026-10-22T00:00:03Z"}),
                json!({"vertex":3,"name":"[3/3] RUN sleep","event":"done","seconds":2.0}),
                json!({"vertex":2,"name":make,"event":"error","seconds":2.0,"message":"exit code: 2"}),
                json!({"event":"message","text":"error: failed to solve: exit code: 2"}),
            ]
        );
        assert_eq!(
            renderer.last_error.as_deref(),
            Some("failed to solve: exit code: 2")
        );
    }

    #[test]
    fn a_second_run_continues_the_numbering() {
        let first = r#"{"vertexes":[{"digest":"sha256:a","name":"load metadata","started":"2026-10-22T00:00:00Z","completed":"2026-10-22T00:00:01Z"}]}"#;
        let second = r#"{"vertexes":[{"digest":"sha256:a","name":"load metadata","started":"2026-10-22T00:01:00Z","completed":"2026-10-22T00:01:01Z"},{"digest":"sha256:b","name":"RUN x","cached":true}]}"#;
        let mut renderer = Renderer::default();
        render(&mut renderer, first);
        renderer.next_run();
        let out = render(&mut renderer, second);
        let vertices: Vec<&Value> = out.iter().map(|e| &e["vertex"]).collect();
        assert_eq!(vertices, [&json!(2), &json!(2), &json!(3)]);
        assert_eq!(out[1]["event"], "done");
    }
}
