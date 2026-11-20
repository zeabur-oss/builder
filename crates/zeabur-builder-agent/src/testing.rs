//! Test doubles: a temporary directory, a model API that replays responses, and a scripted
//! trial builder.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use rig_core::DynModel;
use rig_core::catalog::Catalog;
use rig_core::http_client::{
    Error, HeaderMap, HttpClientExt, LazyBody, MultipartForm, Request, Response, Result,
    StatusCode, StreamingResponse,
};
use rig_core::operation::Completion;
use rig_core::providers::registry::ConnectOptions;

use crate::trial::{Attempt, TrialBuild};

static NEXT: AtomicU32 = AtomicU32::new(0);

/// A new empty directory.
#[derive(Debug)]
pub(crate) struct Dir(PathBuf);

impl Dir {
    pub(crate) fn new() -> Self {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("zeabur-builder-agent-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    /// Writes `text` to `relative`, creating its directories.
    pub(crate) fn write(&self, relative: &str, text: &str) {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap_or_default();
    }
}

/// A model API that answers with prepared response bodies, in order, and keeps the requests.
#[derive(Debug, Clone, Default)]
pub(crate) struct Replay {
    responses: Arc<Mutex<VecDeque<String>>>,
    pub(crate) requests: Arc<Mutex<Vec<String>>>,
    /// The headers of each request.
    pub(crate) headers: Arc<Mutex<Vec<HeaderMap>>>,
}

impl Replay {
    pub(crate) fn new(responses: impl IntoIterator<Item = String>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into_iter().collect())),
            requests: Arc::default(),
            headers: Arc::default(),
        }
    }

    /// A Claude model that sends through this replay.
    pub(crate) fn model(&self) -> DynModel<Completion> {
        Catalog::builtin()
            .connect_with(
                "anthropic/claude-opus-5-5",
                ConnectOptions::new().api_key("test").http(self.clone()),
            )
            .unwrap()
    }

    fn next(&self, body: &Bytes) -> Option<Bytes> {
        self.requests
            .lock()
            .unwrap()
            .push(String::from_utf8_lossy(body).into_owned());
        self.responses.lock().unwrap().pop_front().map(Bytes::from)
    }
}

impl HttpClientExt for Replay {
    fn send<T, U>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = Result<Response<LazyBody<U>>>> + Send + 'static
    where
        T: Into<Bytes> + Send,
        U: From<Bytes> + Send + 'static,
    {
        self.headers.lock().unwrap().push(req.headers().clone());
        let reply = self.next(&req.into_body().into());
        async move {
            let reply = reply.ok_or_else(|| {
                Error::non_success_with_details(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    HeaderMap::new(),
                    "no more replies".to_owned(),
                )
            })?;
            let body: LazyBody<U> = Box::pin(async move { Ok(U::from(reply)) });
            Ok(Response::builder()
                .header("content-type", "application/json")
                .body(body)?)
        }
    }

    fn send_multipart<U>(
        &self,
        _req: Request<MultipartForm>,
    ) -> impl Future<Output = Result<Response<LazyBody<U>>>> + Send + 'static
    where
        U: From<Bytes> + Send + 'static,
    {
        std::future::ready(Err(Error::StreamEnded))
    }

    fn send_streaming<T>(
        &self,
        _req: Request<T>,
    ) -> impl Future<Output = Result<StreamingResponse>> + Send
    where
        T: Into<Bytes> + Send,
    {
        std::future::ready(Err(Error::StreamEnded))
    }
}

/// A Messages API reply that calls `tool` with `input`.
pub(crate) fn tool_use(id: &str, tool: &str, input: &impl serde::Serialize) -> String {
    serde_json::json!({
        "id": format!("msg_{id}"),
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-5-5",
        "content": [{"type": "tool_use", "id": format!("toolu_{id}"), "name": tool, "input": input}],
        "stop_reason": "tool_use",
        "stop_sequence": null,
        "usage": {"input_tokens": 1000, "output_tokens": 100}
    })
    .to_string()
}

/// A Messages API reply with only text.
pub(crate) fn text(id: &str, text: &str) -> String {
    serde_json::json!({
        "id": format!("msg_{id}"),
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-5-5",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 1000, "output_tokens": 100}
    })
    .to_string()
}

/// A trial builder that answers with prepared attempts, in order, and keeps the Dockerfiles. With
/// no attempt left, a build never ends.
#[derive(Debug, Clone, Default)]
pub(crate) struct Script {
    attempts: Arc<Mutex<VecDeque<Attempt>>>,
    pub(crate) built: Arc<Mutex<Vec<String>>>,
}

impl Script {
    pub(crate) fn new(attempts: impl IntoIterator<Item = Attempt>) -> Self {
        Self {
            attempts: Arc::new(Mutex::new(attempts.into_iter().collect())),
            built: Arc::default(),
        }
    }
}

impl TrialBuild for Script {
    fn build(&self, dockerfile: &str, _dockerignore: &str) -> impl Future<Output = Attempt> + Send {
        self.built.lock().unwrap().push(dockerfile.to_owned());
        let next = self.attempts.lock().unwrap().pop_front();
        async move {
            match next {
                Some(attempt) => attempt,
                None => std::future::pending().await,
            }
        }
    }
}

/// A failed attempt.
pub(crate) fn failed(summary: &str) -> Attempt {
    Attempt::Failed {
        summary: summary.to_owned(),
        log: format!("#[2/2] RUN npm ci\n{summary}"),
    }
}
