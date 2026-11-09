//! Fixture: a command takes an HTTP request, renamed on import, as its input.

use http::Request as HttpRequest;

pub struct Apply;

impl Apply {
    pub fn run(&self, request: &HttpRequest<()>) -> bool {
        request.method() == "POST"
    }
}
