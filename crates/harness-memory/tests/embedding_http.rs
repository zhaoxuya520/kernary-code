use harness_auth::{CredentialId, CredentialStore, MemoryCredentialStore, SecretString};
use harness_http::{
    HttpBody, HttpTransportError, StreamingHttpRequest, StreamingHttpResponse,
    StreamingHttpTransport,
};
use harness_memory::*;
use std::io::{BufReader, Cursor};
use std::sync::{Arc, Mutex};

struct MockTransport {
    captured: Mutex<Option<serde_json::Value>>,
}
impl StreamingHttpTransport for MockTransport {
    fn send(
        &self,
        request: StreamingHttpRequest,
    ) -> Result<StreamingHttpResponse, HttpTransportError> {
        if let HttpBody::Json(body) = request.body {
            *self.captured.lock().expect("capture") = Some(body);
        }
        Ok(StreamingHttpResponse {
            status: 200,
            headers: Default::default(),
            body: Box::new(BufReader::new(Cursor::new(
                br#"{"object":"list","data":[{"index":0,"embedding":[1.0,0.0,0.5]}]}"#.to_vec(),
            ))),
        })
    }
}

#[test]
fn http_embedding_uses_official_shape_and_os_credential() {
    let credentials = Arc::new(MemoryCredentialStore::new());
    credentials
        .put(
            &CredentialId::new("embedding:test"),
            SecretString::new("secret"),
        )
        .expect("credential");
    let transport = Arc::new(MockTransport {
        captured: Mutex::new(None),
    });
    let factory = HttpEmbeddingFactory::new(
        HttpEmbeddingConfig {
            provider: "openai".to_owned(),
            endpoint: "https://api.openai.com/v1/embeddings".to_owned(),
            credential_id: Some("embedding:test".to_owned()),
            allow_remote_project_private: true,
            timeout_millis: Some(1000),
        },
        credentials,
        transport.clone(),
    )
    .expect("factory");
    let provider = factory
        .create(&EmbeddingProfile {
            model: "text-embedding-3-small".to_owned(),
            provider: "openai".to_owned(),
            dimensions: 3,
        })
        .expect("provider");
    assert_eq!(
        provider.embed("project memory").expect("embed"),
        vec![1.0, 0.0, 0.5]
    );
    let body = transport
        .captured
        .lock()
        .expect("captured")
        .clone()
        .expect("body");
    assert_eq!(body["model"], "text-embedding-3-small");
    assert_eq!(body["dimensions"], 3);
    assert_eq!(body["encoding_format"], "float");
}

#[test]
fn remote_embedding_requires_explicit_project_private_egress() {
    assert!(
        HttpEmbeddingFactory::new(
            HttpEmbeddingConfig {
                provider: "openai".to_owned(),
                endpoint: "https://api.openai.com/v1/embeddings".to_owned(),
                credential_id: None,
                allow_remote_project_private: false,
                timeout_millis: None
            },
            Arc::new(MemoryCredentialStore::new()),
            Arc::new(MockTransport {
                captured: Mutex::new(None)
            })
        )
        .is_err()
    );
}
