//! HTTP transport abstraction.
//!
//! The runtime is generic over `HttpTransport` so tests can inject mocks and
//! production builds can pick `reqwest` (native) or `fetch` (wasm) without
//! changing call sites.
//!
//! This module also provides [`MockTransport`], a test double that matches
//! requests to queued canned responses.  It is always compiled (not gated
//! behind `#[cfg(test)]`) so the differential harness and future mock server
//! can use it from outside unit-test contexts.

use std::future::Future;
use std::pin::Pin;

// ---------------------------------------------------------------------------
// TransportError
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    Network(String),
    Status(u16),
    Serialization(String),
}

// ---------------------------------------------------------------------------
// HttpTransport trait
// ---------------------------------------------------------------------------

/// Boxed future returned by [`HttpTransport::post_form`].
///
/// On native targets we require `+ Send` so the achievement runtime can be
/// driven from a multi-thread Tokio runtime (the iOS / desktop FFI spawns
/// per-frame evaluation tasks). On wasm we drop the bound — `web_sys`
/// request/response types are deliberately `!Send`, and wasm is
/// single-threaded anyway, so `Send` would be both unsatisfiable and
/// pointless there.
#[cfg(not(target_arch = "wasm32"))]
pub type TransportFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<u8>, TransportError>> + Send + 'a>>;
#[cfg(target_arch = "wasm32")]
pub type TransportFuture<'a> = Pin<Box<dyn Future<Output = Result<Vec<u8>, TransportError>> + 'a>>;

/// Async HTTP transport. One method: POST form-urlencoded, return bytes.
///
/// Native builds can use [`ReqwestTransport`]; wasm builds can provide a fetch
/// wrapper behind the `http-fetch` feature without changing runtime code.
pub trait HttpTransport {
    /// POST a form-urlencoded body and return the raw response bytes.
    fn post_form<'a>(
        &'a mut self,
        url: &'a str,
        form: &'a [(&'a str, &'a str)],
    ) -> TransportFuture<'a>;
}

// ---------------------------------------------------------------------------
// MockTransport
// ---------------------------------------------------------------------------

/// A queued-expectation test double for [`HttpTransport`].
///
/// Expectations are consumed in order: the first expectation whose
/// `url_contains` is a substring of the request URL *and* whose `match_form`
/// key-value pairs all appear in the request form is returned and removed.
///
/// In strict mode (the default) an unmatched request returns a
/// [`TransportError::Network`] containing a diagnostic.  In non-strict mode
/// it returns empty bytes.
pub struct MockTransport {
    pub expectations: Vec<MockExpectation>,
    pub calls: Vec<MockCall>,
    pub strict: bool,
}

/// A canned response entry for [`MockTransport`].
#[derive(Debug, Clone)]
pub struct MockExpectation {
    /// The request URL must contain this substring.
    pub url_contains: String,
    /// Every (key, value) pair here must appear in the incoming form.
    pub match_form: Vec<(String, String)>,
    /// Bytes to return when the expectation matches.
    pub response: Vec<u8>,
}

/// A record of one call made through [`MockTransport`].
#[derive(Debug, Clone)]
pub struct MockCall {
    pub url: String,
    pub form: Vec<(String, String)>,
}

impl MockTransport {
    /// Create a new strict `MockTransport` with no expectations.
    pub fn new() -> Self {
        Self {
            expectations: vec![],
            calls: vec![],
            strict: true,
        }
    }

    /// Queue a canned expectation.
    ///
    /// `url_contains` — substring that must appear in the request URL.
    /// `match_form`   — subset of form key-value pairs that must all be present.
    /// `response`     — raw bytes to return on a match.
    pub fn expect(
        &mut self,
        url_contains: &str,
        match_form: &[(&str, &str)],
        response: impl Into<Vec<u8>>,
    ) {
        self.expectations.push(MockExpectation {
            url_contains: url_contains.to_string(),
            match_form: match_form
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            response: response.into(),
        });
    }

    /// Convenience wrapper: serialize `body` as JSON and queue it.
    pub fn expect_json(
        &mut self,
        url_contains: &str,
        match_form: &[(&str, &str)],
        body: serde_json::Value,
    ) {
        self.expect(url_contains, match_form, body.to_string().into_bytes());
    }

    /// Panic with a diagnostic if no recorded call matches `url_substr` and
    /// all of `form_subset`.
    pub fn assert_called(&self, url_substr: &str, form_subset: &[(&str, &str)]) {
        let matched = self.calls.iter().any(|c| {
            c.url.contains(url_substr)
                && form_subset
                    .iter()
                    .all(|(k, v)| c.form.iter().any(|(fk, fv)| fk == k && fv == v))
        });
        if !matched {
            panic!(
                "MockTransport: assert_called failed.\n  expected url_substr={:?} form_subset={:?}\n  recorded calls: {:#?}",
                url_substr, form_subset, self.calls
            );
        }
    }

    /// Count how many recorded calls contain `url_substr` in their URL.
    pub fn call_count(&self, url_substr: &str) -> usize {
        self.calls
            .iter()
            .filter(|c| c.url.contains(url_substr))
            .count()
    }
}

impl Default for MockTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpTransport for MockTransport {
    fn post_form<'a>(
        &'a mut self,
        url: &'a str,
        form: &'a [(&'a str, &'a str)],
    ) -> TransportFuture<'a> {
        Box::pin(async move {
            // Record the call.
            self.calls.push(MockCall {
                url: url.to_string(),
                form: form
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            });

            // Find the first matching expectation.
            let pos = self.expectations.iter().position(|exp| {
                url.contains(&exp.url_contains)
                    && exp
                        .match_form
                        .iter()
                        .all(|(ek, ev)| form.iter().any(|(fk, fv)| fk == ek && fv == ev))
            });

            match pos {
                Some(idx) => {
                    let exp = self.expectations.remove(idx);
                    Ok(exp.response)
                }
                None => {
                    if self.strict {
                        Err(TransportError::Network(format!(
                            "no mock expectation matched url={url:?} form={:?}",
                            form.iter()
                                .map(|(k, v)| format!("{k}={v}"))
                                .collect::<Vec<_>>()
                        )))
                    } else {
                        Ok(vec![])
                    }
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// FetchTransport (wasm — browser fetch API)
// ---------------------------------------------------------------------------

#[cfg(feature = "http-fetch")]
pub struct FetchTransport {
    extra_headers: Vec<(String, String)>,
}

#[cfg(feature = "http-fetch")]
impl FetchTransport {
    pub fn new() -> Self {
        Self {
            extra_headers: Vec::new(),
        }
    }

    /// Add an extra header sent with every request (e.g. `Authorization`).
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_headers.push((name.into(), value.into()));
        self
    }
}

#[cfg(feature = "http-fetch")]
impl Default for FetchTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "http-fetch")]
impl HttpTransport for FetchTransport {
    fn post_form<'a>(
        &'a mut self,
        url: &'a str,
        form: &'a [(&'a str, &'a str)],
    ) -> TransportFuture<'a> {
        Box::pin(async move {
            use wasm_bindgen::JsCast;

            let body = form
                .iter()
                .map(|(k, v)| format!("{}={}", form_encode(k), form_encode(v)))
                .collect::<Vec<_>>()
                .join("&");

            let opts = web_sys::RequestInit::new();
            opts.set_method("POST");
            opts.set_body(&wasm_bindgen::JsValue::from_str(&body));

            let headers = web_sys::Headers::new()
                .map_err(|e| TransportError::Network(format!("{e:?}")))?;
            headers
                .set("Content-Type", "application/x-www-form-urlencoded")
                .map_err(|e| TransportError::Network(format!("{e:?}")))?;
            for (name, value) in &self.extra_headers {
                headers
                    .set(name, value)
                    .map_err(|e| TransportError::Network(format!("{e:?}")))?;
            }
            opts.set_headers(&headers);

            let request = web_sys::Request::new_with_str_and_init(url, &opts)
                .map_err(|e| TransportError::Network(format!("{e:?}")))?;

            let window = web_sys::window()
                .ok_or_else(|| TransportError::Network("no global window".into()))?;
            let resp_val = wasm_bindgen_futures::JsFuture::from(window.fetch_with_request(&request))
                .await
                .map_err(|e| TransportError::Network(format!("{e:?}")))?;
            let resp: web_sys::Response = resp_val
                .dyn_into()
                .map_err(|e| TransportError::Network(format!("{e:?}")))?;

            if !resp.ok() {
                return Err(TransportError::Status(resp.status()));
            }

            let buf = resp
                .array_buffer()
                .map_err(|e| TransportError::Network(format!("{e:?}")))?;
            let buf = wasm_bindgen_futures::JsFuture::from(buf)
                .await
                .map_err(|e| TransportError::Network(format!("{e:?}")))?;
            let bytes = js_sys::Uint8Array::new(&buf);
            Ok(bytes.to_vec())
        })
    }
}

/// Minimal `application/x-www-form-urlencoded` encoder (no external deps).
#[cfg(feature = "http-fetch")]
fn form_encode(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0xf) as usize] as char);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// ReqwestTransport
// ---------------------------------------------------------------------------

/// Blocking HTTP transport backed by [`reqwest::blocking::Client`].
///
/// Compiled only when the `http-reqwest` feature is enabled.
#[cfg(feature = "http-reqwest")]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

#[cfg(feature = "http-reqwest")]
impl ReqwestTransport {
    /// Build a new transport with a default reqwest client.
    pub fn new() -> Result<Self, TransportError> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| TransportError::Network(e.to_string()))?;
        Ok(Self { client })
    }

    /// Wrap an existing `reqwest::blocking::Client` (useful for tests that
    /// want custom timeouts, certificates, etc.).
    pub fn from_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

#[cfg(feature = "http-reqwest")]
impl HttpTransport for ReqwestTransport {
    fn post_form<'a>(
        &'a mut self,
        url: &'a str,
        form: &'a [(&'a str, &'a str)],
    ) -> TransportFuture<'a> {
        Box::pin(async move {
            let resp = self
                .client
                .post(url)
                .form(form)
                .send()
                .await
                .map_err(|e| TransportError::Network(e.to_string()))?;
            let status = resp.status();
            if !status.is_success() {
                return Err(TransportError::Status(status.as_u16()));
            }
            resp.bytes()
                .await
                .map(|b| b.to_vec())
                .map_err(|e| TransportError::Network(e.to_string()))
        })
    }
}

#[cfg(feature = "http-reqwest")]
impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new().expect("reqwest client build")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // Test 1: Single expectation + matching call returns the response.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn single_expectation_matched_returns_response() {
        let mut t = MockTransport::new();
        t.expect("login2", &[("r", "login2")], b"ok".to_vec());

        let resp = t
            .post_form("http://example.com/login2", &[("r", "login2")])
            .await
            .unwrap();
        assert_eq!(resp, b"ok");
    }

    // -----------------------------------------------------------------------
    // Test 2: Multiple sequential expectations consumed in order.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn multiple_expectations_consumed_in_order() {
        let mut t = MockTransport::new();
        t.expect("login2", &[], b"first".to_vec());
        t.expect("login2", &[], b"second".to_vec());

        let r1 = t.post_form("http://example.com/login2", &[]).await.unwrap();
        let r2 = t.post_form("http://example.com/login2", &[]).await.unwrap();
        assert_eq!(r1, b"first");
        assert_eq!(r2, b"second");
    }

    // -----------------------------------------------------------------------
    // Test 3: Form subset matching ignores extra fields in the call.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn form_subset_match_ignores_extra_fields() {
        let mut t = MockTransport::new();
        // Expectation only cares about key "r".
        t.expect("endpoint", &[("r", "gameid")], b"matched".to_vec());

        // Call includes extra "m" field which should be fine.
        let resp = t
            .post_form(
                "http://example.com/endpoint",
                &[("r", "gameid"), ("m", "abc123")],
            )
            .await
            .unwrap();
        assert_eq!(resp, b"matched");
    }

    // -----------------------------------------------------------------------
    // Test 4: Missing expectation in strict mode → Network error.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn strict_mode_no_match_returns_network_error() {
        let mut t = MockTransport::new();
        // No expectations queued.

        let err = t
            .post_form("http://example.com/foo", &[("r", "ping")])
            .await
            .unwrap_err();
        assert!(matches!(err, TransportError::Network(_)));
    }

    // -----------------------------------------------------------------------
    // Test 5: assert_called panics on no match; passes on match.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn assert_called_passes_when_call_recorded() {
        let mut t = MockTransport::new();
        t.strict = false; // non-strict so we don't need to queue expectations

        t.post_form("http://example.com/award", &[("r", "award"), ("a", "42")])
            .await
            .unwrap();

        // Should not panic.
        t.assert_called("award", &[("r", "award"), ("a", "42")]);
    }

    #[test]
    #[should_panic(expected = "assert_called failed")]
    fn assert_called_panics_when_no_matching_call() {
        let t = MockTransport::new();
        // No calls have been made.
        t.assert_called("award", &[("r", "award")]);
    }

    // -----------------------------------------------------------------------
    // Test 6: call_count returns correct count.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn call_count_counts_url_matches() {
        let mut t = MockTransport::new();
        t.strict = false;

        t.post_form("http://example.com/ping", &[]).await.unwrap();
        t.post_form("http://example.com/ping", &[]).await.unwrap();
        t.post_form("http://example.com/award", &[]).await.unwrap();

        assert_eq!(t.call_count("ping"), 2);
        assert_eq!(t.call_count("award"), 1);
        assert_eq!(t.call_count("gameid"), 0);
    }

    // -----------------------------------------------------------------------
    // Test 7: Non-strict mode returns empty bytes on no match.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn non_strict_returns_empty_on_no_match() {
        let mut t = MockTransport::new();
        t.strict = false;

        let resp = t
            .post_form("http://example.com/whatever", &[("r", "unknown")])
            .await
            .unwrap();
        assert!(resp.is_empty());
    }

    // -----------------------------------------------------------------------
    // Test 8: expect_json convenience helper.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn expect_json_returns_json_bytes() {
        let mut t = MockTransport::new();
        t.expect_json("login2", &[], json!({"Success": true, "Token": "tok"}));

        let bytes = t.post_form("http://example.com/login2", &[]).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["Token"], "tok");
    }

    // -----------------------------------------------------------------------
    // Test 9 (http-reqwest feature only): ReqwestTransport::new() succeeds.
    // -----------------------------------------------------------------------

    #[cfg(feature = "http-reqwest")]
    #[test]
    fn reqwest_transport_new_does_not_panic() {
        let _t = super::ReqwestTransport::new().expect("ReqwestTransport::new() should succeed");
    }
}
