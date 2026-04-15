//! HTTP transport abstraction.
//!
//! The runtime is generic over `HttpTransport` so tests can inject mocks and
//! production builds can pick `reqwest` (native) or `fetch` (wasm) without
//! changing call sites.

pub trait HttpTransport {
    // TODO: async post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<Vec<u8>>;
}
