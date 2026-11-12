//! Deliberately broken fakes (ver §5): each bends the good fake's traffic the way a wrong
//! server would, and `tests/wire/contract.rs` proves the suite fails the cases named on each.

use std::sync::Mutex;

use serde_json::Value;

use super::{WireRequest, WireResponse, WireTarget};

fn is_session_create(request: &WireRequest) -> bool {
    request.method == "POST"
        && super::path_of(&request.url)
            .split('?')
            .next()
            .is_some_and(|p| p.ends_with("/sessions"))
}

fn is_poll(request: &WireRequest) -> bool {
    request.method == "GET"
        && super::path_of(&request.url)
            .split('?')
            .next()
            .is_some_and(|p| p.ends_with("/messages"))
}

/// Steals a live session instead of answering 409: deletes the session it saw opened and opens
/// anew. Fails WC-03.
#[derive(Debug)]
pub struct SessionStealing<T> {
    inner: T,
    opened: Mutex<Vec<(String, String)>>,
}

impl<T> SessionStealing<T> {
    /// Wraps `inner`.
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            opened: Mutex::new(Vec::new()),
        }
    }
}

impl<T: WireTarget> WireTarget for SessionStealing<T> {
    async fn send(&self, request: WireRequest) -> Result<WireResponse, String> {
        let create = is_session_create(&request);
        let answer = self.inner.send(request.clone()).await?;
        if !create {
            return Ok(answer);
        }
        let base = request.url.split('?').next().unwrap_or_default().to_owned();
        if answer.status == 200 {
            if let Some(id) = answer.json()["sessionId"].as_str() {
                self.opened
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((base, id.to_owned()));
            }
            return Ok(answer);
        }
        let stolen: Vec<String> = self
            .opened
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(b, _)| *b == base)
            .map(|(_, id)| id.clone())
            .collect();
        for id in stolen {
            let mut delete = request.clone();
            delete.method = "DELETE".to_owned();
            delete.url = request
                .url
                .replacen("/sessions", &format!("/sessions/{id}"), 1);
            delete.body = Vec::new();
            self.inner.send(delete).await?;
        }
        self.inner.send(request).await
    }
}

/// Acks every message as it hands it out, so nothing is ever redelivered. Fails WC-06, and
/// WC-07 and WC-15, whose own acks then find nothing.
#[derive(Debug)]
pub struct AckingOnPoll<T>(pub T);

impl<T: WireTarget> WireTarget for AckingOnPoll<T> {
    async fn send(&self, request: WireRequest) -> Result<WireResponse, String> {
        let poll = is_poll(&request);
        let answer = self.0.send(request.clone()).await?;
        if poll
            && answer.status == 200
            && let Some(id) = answer.json()["messageId"].as_u64()
        {
            let mut ack = request;
            ack.method = "DELETE".to_owned();
            ack.url = format!("{}/{id}", ack.url.split('?').next().unwrap_or_default());
            self.0.send(ack).await?;
        }
        Ok(answer)
    }
}

/// Ignores the advertised capacity, assigning as if it were large. Fails WC-08.
#[derive(Debug)]
pub struct CapacityIgnoring<T>(pub T);

impl<T: WireTarget> WireTarget for CapacityIgnoring<T> {
    async fn send(&self, mut request: WireRequest) -> Result<WireResponse, String> {
        for (name, value) in &mut request.headers {
            if name.eq_ignore_ascii_case("x-scalesetmaxcapacity") {
                *value = "1000".to_owned();
            }
        }
        self.0.send(request).await
    }
}

/// Answers errors without their `typeName`, by status alone. Fails WC-03, WC-04, WC-10, WC-12.
#[derive(Debug)]
pub struct TypeNameDropping<T>(pub T);

impl<T: WireTarget> WireTarget for TypeNameDropping<T> {
    async fn send(&self, request: WireRequest) -> Result<WireResponse, String> {
        let mut answer = self.0.send(request).await?;
        if answer.status >= 400
            && let Value::Object(mut body) = answer.json()
        {
            body.remove("typeName");
            answer.body = Value::Object(body).to_string().into_bytes();
        }
        Ok(answer)
    }
}

/// Keeps only the first ten job messages of each message and drops the rest: a backlog lost
/// instead of carried into the next message. Fails WC-07.
#[derive(Debug)]
pub struct Truncating<T>(pub T);

impl<T: WireTarget> WireTarget for Truncating<T> {
    async fn send(&self, request: WireRequest) -> Result<WireResponse, String> {
        let poll = is_poll(&request);
        let mut answer = self.0.send(request).await?;
        if poll && answer.status == 200 {
            let mut body = answer.json();
            let jobs: Option<Vec<Value>> = body["body"]
                .as_str()
                .and_then(|inner| serde_json::from_str(inner).ok());
            if let Some(mut jobs) = jobs {
                jobs.truncate(10);
                body["body"] = Value::String(Value::Array(jobs).to_string());
                answer.body = body.to_string().into_bytes();
            }
        }
        Ok(answer)
    }
}
