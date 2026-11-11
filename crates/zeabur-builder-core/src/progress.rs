//! The `buildctl build --progress rawjson` stream: its lines, cache statistics and steps.
//!
//! In `rawjson` mode buildctl writes each `client.SolveStatus` as one JSON object per line on
//! stderr ([progressui], `RawJSONMode`). A vertex is reported many times as it starts and
//! completes; it counts once, by digest, and counts as cached when its completed report says
//! `cached: true` (`client.Vertex`, [graph.go]). Lines that are not JSON (buildctl's own messages)
//! are ignored, so the counter can read the whole stream.
//!
//! [`StepLog`] keeps one [`Step`] per vertex of each `buildctl` run, in the order the vertices
//! first appear, with the `started`, `completed`, `cached` and `error` of their reports.
//!
//! [progressui]: https://github.com/moby/buildkit/blob/v0.34.1/util/progress/progressui/display.go
//! [graph.go]: https://github.com/moby/buildkit/blob/v0.34.1/client/graph.go

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::result::{CacheStats, Step, StepState};

/// One line of the stream: a `client.SolveStatus`.
#[derive(Debug, Deserialize)]
pub struct SolveStatus {
    /// Steps that started, completed, were cached or failed since the last line.
    #[serde(default)]
    pub vertexes: Vec<Vertex>,
    /// Output of the steps since the last line.
    #[serde(default)]
    pub logs: Vec<Log>,
}

impl SolveStatus {
    /// The status on one line, or `None` for buildctl's own messages.
    #[must_use]
    pub fn parse(line: &str) -> Option<Self> {
        serde_json::from_str(line).ok()
    }
}

/// One report of a build step.
#[derive(Debug, Deserialize)]
pub struct Vertex {
    /// The step's identity within one `buildctl` run.
    pub digest: String,
    /// Such as `[2/3] RUN npm ci`.
    #[serde(default)]
    pub name: String,
    /// RFC 3339.
    #[serde(default)]
    pub started: Option<String>,
    /// RFC 3339.
    #[serde(default)]
    pub completed: Option<String>,
    /// Whether the result came from the cache.
    #[serde(default)]
    pub cached: bool,
    /// Why the step failed.
    #[serde(default)]
    pub error: Option<String>,
}

/// Output of a step.
#[derive(Debug, Deserialize)]
pub struct Log {
    /// The [`Vertex::digest`] of the step.
    pub vertex: String,
    /// 1 for stdout, 2 for stderr (`util/progress/logs/logs.go`).
    #[serde(default)]
    pub stream: u8,
    /// Base64.
    #[serde(default)]
    pub data: String,
}

/// Counts distinct and cached build steps.
#[derive(Debug, Clone, Default)]
pub struct CacheCounter {
    /// Digest → cached, for vertices seen completed.
    completed: BTreeMap<String, bool>,
}

impl CacheCounter {
    /// Reads one line of the progress stream.
    pub fn line(&mut self, line: &str) {
        let Some(status) = SolveStatus::parse(line) else {
            return;
        };
        for vertex in status.vertexes {
            if vertex.completed.is_some() || vertex.cached {
                let cached = self.completed.entry(vertex.digest).or_insert(false);
                *cached |= vertex.cached;
            }
        }
    }

    /// The totals so far.
    #[must_use]
    pub fn stats(&self) -> CacheStats {
        let total = u32::try_from(self.completed.len()).unwrap_or(u32::MAX);
        let cached =
            u32::try_from(self.completed.values().filter(|c| **c).count()).unwrap_or(u32::MAX);
        CacheStats { total, cached }
    }
}

/// The steps of a build, over all its `buildctl` runs.
#[derive(Debug, Clone, Default)]
pub struct StepLog {
    steps: Vec<Step>,
    /// Digest → index in `steps`, for the current run.
    run: BTreeMap<String, usize>,
    /// The index of the first step that failed.
    failed: Option<usize>,
}

impl StepLog {
    /// Starts the next `buildctl` run: a vertex seen again is a new step.
    pub fn next_run(&mut self) {
        self.run.clear();
    }

    /// Reads one line of the progress stream.
    pub fn line(&mut self, line: &str) {
        let Some(status) = SolveStatus::parse(line) else {
            return;
        };
        for vertex in status.vertexes {
            let index = *self.run.entry(vertex.digest).or_insert_with(|| {
                self.steps.push(Step::default());
                self.steps.len() - 1
            });
            let Some(step) = self.steps.get_mut(index) else {
                continue;
            };
            if step.state != StepState::Running {
                continue;
            }
            if !vertex.name.is_empty() {
                step.name = vertex.name;
            }
            if step.start_time.is_none() {
                step.start_time = vertex.started.as_deref().map(seconds);
            }
            step.cached |= vertex.cached;
            if let Some(error) = vertex.error {
                step.state = StepState::Failed;
                step.message = error;
                self.failed.get_or_insert(index);
            } else if vertex.cached || vertex.completed.is_some() {
                step.state = StepState::Succeeded;
            }
            if step.state != StepState::Running {
                step.completion_time = vertex.completed.as_deref().map(seconds);
            }
        }
    }

    /// Every step, oldest first, and the first that failed.
    #[must_use]
    pub fn finish(self) -> (Vec<Step>, Option<Step>) {
        let failed = self.failed.and_then(|i| self.steps.get(i).cloned());
        (self.steps, failed)
    }
}

/// An RFC 3339 time without its fraction of a second: `2026-10-22T00:00:01.5Z` becomes
/// `2026-10-22T00:00:01Z`.
fn seconds(time: &str) -> String {
    let Some((whole, rest)) = time.split_once('.') else {
        return time.to_owned();
    };
    let zone = rest.trim_start_matches(|c: char| c.is_ascii_digit());
    format!("{whole}{zone}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_each_vertex_once() {
        let stream = r#"
#1 [internal] load build definition
{"vertexes":[{"digest":"sha256:a","name":"[1/3] FROM node","started":"2026-10-22T00:00:00Z"}]}
{"vertexes":[{"digest":"sha256:a","name":"[1/3] FROM node","started":"2026-10-22T00:00:00Z","completed":"2026-10-22T00:00:01Z","cached":true}]}
{"vertexes":[{"digest":"sha256:b","name":"[2/3] RUN npm ci","completed":"2026-10-22T00:00:09Z"}],"logs":[{"vertex":"sha256:b","data":"YWRkZWQ=","timestamp":"2026-10-22T00:00:02Z"}]}
{"vertexes":[{"digest":"sha256:c","name":"[3/3] COPY . .","cached":true}]}
{"statuses":[{"id":"x","current":1,"timestamp":"2026-10-22T00:00:02Z"}]}
not json
"#;
        let mut counter = CacheCounter::default();
        for line in stream.lines() {
            counter.line(line);
        }
        assert_eq!(
            counter.stats(),
            CacheStats {
                total: 3,
                cached: 2
            }
        );
    }

    #[test]
    fn steps_follow_vertices_in_the_order_they_start() {
        let stream = r#"{"vertexes":[{"digest":"sha256:a","name":"[1/3] FROM node","started":"2026-10-22T00:00:00.123456789Z"}]}
{"vertexes":[{"digest":"sha256:a","name":"[1/3] FROM node","started":"2026-10-22T00:00:00Z","completed":"2026-10-22T00:00:01Z","cached":true}]}
{"vertexes":[{"digest":"sha256:b","name":"[2/3] RUN npm ci","started":"2026-10-22T00:00:01Z"},{"digest":"sha256:c","name":"[3/3] COPY . .","started":"2026-10-22T00:00:01Z"}]}
{"vertexes":[{"digest":"sha256:b","name":"[2/3] RUN npm ci","started":"2026-10-22T00:00:01Z","completed":"2026-10-22T00:00:09.5+00:00","error":"process \"npm ci\" did not complete successfully: exit code: 1"}]}
{"vertexes":[{"digest":"sha256:c","name":"[3/3] COPY . .","completed":"2026-10-22T00:00:09Z","error":"context canceled"}]}
not json"#;
        let mut log = StepLog::default();
        for line in stream.lines() {
            log.line(line);
        }
        let (steps, failed) = log.finish();
        let npm = Step {
            name: "[2/3] RUN npm ci".to_owned(),
            state: StepState::Failed,
            cached: false,
            start_time: Some("2026-10-22T00:00:01Z".to_owned()),
            completion_time: Some("2026-10-22T00:00:09+00:00".to_owned()),
            message: r#"process "npm ci" did not complete successfully: exit code: 1"#.to_owned(),
        };
        assert_eq!(
            steps,
            [
                Step {
                    name: "[1/3] FROM node".to_owned(),
                    state: StepState::Succeeded,
                    cached: true,
                    start_time: Some("2026-10-22T00:00:00Z".to_owned()),
                    completion_time: Some("2026-10-22T00:00:01Z".to_owned()),
                    message: String::new(),
                },
                npm.clone(),
                Step {
                    name: "[3/3] COPY . .".to_owned(),
                    state: StepState::Failed,
                    cached: false,
                    start_time: Some("2026-10-22T00:00:01Z".to_owned()),
                    completion_time: Some("2026-10-22T00:00:09Z".to_owned()),
                    message: "context canceled".to_owned(),
                },
            ]
        );
        assert_eq!(failed, Some(npm));
    }

    #[test]
    fn a_vertex_of_the_next_run_is_a_new_step() {
        let line = r#"{"vertexes":[{"digest":"sha256:a","name":"load metadata","completed":"2026-10-22T00:00:01Z"}]}"#;
        let mut log = StepLog::default();
        log.line(line);
        log.line(line);
        log.next_run();
        log.line(line);
        let (steps, failed) = log.finish();
        assert_eq!(steps.len(), 2);
        assert!(steps.iter().all(|s| s.state == StepState::Succeeded));
        assert_eq!(failed, None);
    }
}
