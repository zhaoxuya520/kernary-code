#![forbid(unsafe_code)]

//! Provider Adapter 共用的 HTTPS streaming transport。

use std::collections::BTreeMap;
use std::fmt::{Debug, Formatter};
use std::io::{BufRead, BufReader};
use std::time::Duration;

use harness_auth::SecretString;

pub struct SensitiveHeader {
    pub name: String,
    pub value: SecretString,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Delete,
}

pub enum HttpBody {
    Empty,
    Json(serde_json::Value),
    Form(Vec<(String, String)>),
}

impl Debug for SensitiveHeader {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SensitiveHeader")
            .field("name", &self.name)
            .field("value", &"[REDACTED]")
            .finish()
    }
}

pub struct StreamingHttpRequest {
    pub method: HttpMethod,
    pub endpoint: String,
    pub headers: BTreeMap<String, String>,
    pub sensitive_headers: Vec<SensitiveHeader>,
    pub body: HttpBody,
    pub timeout: Duration,
}

impl StreamingHttpRequest {
    #[must_use]
    pub fn json(endpoint: impl Into<String>, body: serde_json::Value, timeout: Duration) -> Self {
        Self {
            method: HttpMethod::Post,
            endpoint: endpoint.into(),
            headers: [
                ("Accept".to_owned(), "text/event-stream".to_owned()),
                ("Content-Type".to_owned(), "application/json".to_owned()),
            ]
            .into_iter()
            .collect(),
            sensitive_headers: vec![],
            body: HttpBody::Json(body),
            timeout,
        }
    }

    #[must_use]
    pub fn get(endpoint: impl Into<String>, timeout: Duration) -> Self {
        Self {
            method: HttpMethod::Get,
            endpoint: endpoint.into(),
            headers: BTreeMap::new(),
            sensitive_headers: vec![],
            body: HttpBody::Empty,
            timeout,
        }
    }

    #[must_use]
    pub fn delete(endpoint: impl Into<String>, timeout: Duration) -> Self {
        Self {
            method: HttpMethod::Delete,
            endpoint: endpoint.into(),
            headers: BTreeMap::new(),
            sensitive_headers: vec![],
            body: HttpBody::Empty,
            timeout,
        }
    }

    #[must_use]
    pub fn form(
        endpoint: impl Into<String>,
        fields: Vec<(String, String)>,
        timeout: Duration,
    ) -> Self {
        Self {
            method: HttpMethod::Post,
            endpoint: endpoint.into(),
            headers: [("Accept".to_owned(), "application/json".to_owned())]
                .into_iter()
                .collect(),
            sensitive_headers: vec![],
            body: HttpBody::Form(fields),
            timeout,
        }
    }

    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    #[must_use]
    pub fn with_sensitive_header(mut self, name: impl Into<String>, value: SecretString) -> Self {
        self.sensitive_headers.push(SensitiveHeader {
            name: name.into(),
            value,
        });
        self
    }
}

impl Debug for StreamingHttpRequest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StreamingHttpRequest")
            .field("method", &self.method)
            .field("endpoint", &self.endpoint)
            .field("headers", &self.headers)
            .field("sensitive_headers", &self.sensitive_headers)
            .field("body", &"[REDACTED]")
            .field("timeout", &self.timeout)
            .finish()
    }
}

pub struct StreamingHttpResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Box<dyn BufRead + Send>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpTransportError {
    pub code: String,
    pub message: String,
    pub timeout: bool,
}

pub trait StreamingHttpTransport: Send + Sync {
    fn send(
        &self,
        request: StreamingHttpRequest,
    ) -> Result<StreamingHttpResponse, HttpTransportError>;
}

#[derive(Clone, Debug)]
pub struct UreqStreamingTransport {
    agent: ureq::Agent,
}

impl Default for UreqStreamingTransport {
    fn default() -> Self {
        use ureq::tls::{RootCerts, TlsConfig};

        Self {
            agent: ureq::Agent::config_builder()
                .http_status_as_error(false)
                .tls_config(
                    TlsConfig::builder()
                        .root_certs(RootCerts::PlatformVerifier)
                        .build(),
                )
                .build()
                .new_agent(),
        }
    }
}

impl StreamingHttpTransport for UreqStreamingTransport {
    fn send(
        &self,
        request: StreamingHttpRequest,
    ) -> Result<StreamingHttpResponse, HttpTransportError> {
        let response = match request.method {
            HttpMethod::Get => {
                configure_builder(self.agent.get(&request.endpoint), &request)?.call()
            }
            HttpMethod::Delete => {
                configure_builder(self.agent.delete(&request.endpoint), &request)?.call()
            }
            HttpMethod::Post => {
                let builder = configure_builder(self.agent.post(&request.endpoint), &request)?;
                match &request.body {
                    HttpBody::Empty => builder.send_empty(),
                    HttpBody::Json(body) => builder.send_json(body),
                    HttpBody::Form(fields) => builder.send_form(
                        fields
                            .iter()
                            .map(|(key, value)| (key.as_str(), value.as_str())),
                    ),
                }
            }
        }
        .map_err(transport_error)?;
        let status = response.status().as_u16();
        let mut headers = BTreeMap::new();
        for name in [
            "retry-after",
            "x-request-id",
            "content-type",
            "mcp-session-id",
            "mcp-protocol-version",
            "www-authenticate",
        ] {
            if let Some(value) = response.headers().get(name)
                && let Ok(value) = value.to_str()
            {
                headers.insert(name.to_owned(), value.to_owned());
            }
        }
        Ok(StreamingHttpResponse {
            status,
            headers,
            body: Box::new(BufReader::new(response.into_body().into_reader())),
        })
    }
}

fn configure_builder<B>(
    builder: ureq::RequestBuilder<B>,
    request: &StreamingHttpRequest,
) -> Result<ureq::RequestBuilder<B>, HttpTransportError> {
    let mut builder = builder
        .config()
        .timeout_global(Some(request.timeout))
        .build();
    for (name, value) in &request.headers {
        builder = builder.header(name, value);
    }
    for header in &request.sensitive_headers {
        builder = builder.header(
            &header.name,
            header
                .value
                .expose_secret()
                .map_err(|error| HttpTransportError {
                    code: error.code,
                    message: error.message,
                    timeout: false,
                })?,
        );
    }
    Ok(builder)
}

fn transport_error(error: ureq::Error) -> HttpTransportError {
    let text = error.to_string();
    let normalized = text.to_ascii_lowercase();
    let timeout = normalized.contains("timed out") || normalized.contains("timeout");
    HttpTransportError {
        code: if timeout {
            "http-timeout".to_owned()
        } else {
            "http-transport".to_owned()
        },
        message: text,
        timeout,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_body_and_sensitive_headers() {
        let request = StreamingHttpRequest::json(
            "https://example.test/v1",
            serde_json::json!({"private":"source"}),
            Duration::from_secs(1),
        )
        .with_sensitive_header("Authorization", SecretString::new("Bearer sk-hidden"));
        let debug = format!("{request:?}");
        assert!(debug.contains("Authorization"));
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("sk-hidden"));
        assert!(!debug.contains("source"));
    }
}
