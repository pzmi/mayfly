//! The registry over a real loopback socket and TLS (integration level): what a client that
//! trusts the registry's CA is served, and that a client trusting another CA is refused.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::Arc;

use mayfly_fake_registry::{FakeRegistry, GuestOs, Images, Served, TartImage};
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

const NAME: &str = "mayfly/runner";

/// An HTTP answer: status, headers (names lowercase) and body.
#[derive(Debug)]
struct Answer {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

fn client(trusted: &[u8]) -> TlsConnector {
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(trusted.to_vec()))
        .expect("a CA certificate");
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
}

/// `method path` over TLS to `registry`, trusting `trusted`; one request per connection.
fn request(
    registry: &FakeRegistry,
    trusted: &[u8],
    method: &str,
    path: &str,
) -> std::io::Result<Answer> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .expect("a runtime");
    runtime.block_on(async {
        let tcp = TcpStream::connect(registry.addr()).await?;
        let name = ServerName::IpAddress(IpAddr::from([127, 0, 0, 1]).into());
        let mut tls = client(trusted).connect(name, tcp).await?;
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            registry.host()
        );
        tls.write_all(head.as_bytes()).await?;
        let mut raw = Vec::new();
        match tls.read_to_end(&mut raw).await {
            // A peer that closes without close_notify after a complete answer.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && !raw.is_empty() => {}
            other => {
                other?;
            }
        }
        Ok(parse(&raw))
    })
}

fn parse(raw: &[u8]) -> Answer {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a header block");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .expect("a status line");
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    Answer {
        status,
        headers,
        body: raw[split + 4..].to_vec(),
    }
}

fn error_code(answer: &Answer) -> String {
    let body: serde_json::Value = serde_json::from_slice(&answer.body).expect("a JSON error");
    body["errors"][0]["code"]
        .as_str()
        .expect("an error code")
        .to_owned()
}

fn published() -> (TartImage, FakeRegistry) {
    let image = TartImage::synthetic(b"runner-image", GuestOs::Linux);
    let registry = FakeRegistry::start(Images::new().publish(NAME, &image)).expect("a registry");
    (image, registry)
}

#[test]
fn should_serve_the_manifests_exact_bytes_when_the_client_trusts_the_registrys_ca() {
    // given
    let (image, registry) = published();
    let path = format!("/v2/{NAME}/manifests/{}", image.digest());

    // when
    let answer = request(&registry, registry.ca_der(), "GET", &path).expect("an answer");

    // then
    assert_eq!(answer.status, 200);
    assert_eq!(answer.body, image.manifest());
    assert_eq!(
        answer
            .headers
            .get("docker-content-digest")
            .map(String::as_str),
        Some(image.digest())
    );
    assert_eq!(
        answer.headers.get("content-type").map(String::as_str),
        Some("application/vnd.oci.image.manifest.v1+json")
    );
    assert_eq!(
        registry.served(),
        [Served {
            method: "GET".to_owned(),
            path,
            status: 200
        }]
    );
}

#[test]
fn should_answer_the_headers_alone_when_a_manifest_is_asked_by_head() {
    // given
    let (image, registry) = published();
    let path = format!("/v2/{NAME}/manifests/{}", image.digest());

    // when
    let answer = request(&registry, registry.ca_der(), "HEAD", &path).expect("an answer");

    // then
    assert_eq!(answer.status, 200);
    assert!(answer.body.is_empty(), "{answer:?}");
    assert_eq!(
        answer.headers.get("content-length"),
        Some(&image.manifest().len().to_string())
    );
}

#[test]
fn should_serve_every_blob_the_manifest_names_when_asked_by_digest() {
    // given
    let (image, registry) = published();

    for (digest, blob) in image.blobs() {
        // when
        let answer = request(
            &registry,
            registry.ca_der(),
            "GET",
            &format!("/v2/{NAME}/blobs/{digest}"),
        )
        .expect("an answer");

        // then
        assert_eq!(answer.status, 200, "{digest}");
        assert_eq!(&answer.body, blob, "{digest}");
    }
}

#[test]
fn should_answer_an_oci_error_when_the_reference_is_not_published() {
    // given
    let (image, registry) = published();
    let unknown = format!("sha256:{}", "0".repeat(64));
    let cases = [
        (
            format!("/v2/{NAME}/manifests/{unknown}"),
            "MANIFEST_UNKNOWN",
        ),
        (format!("/v2/{NAME}/manifests/latest"), "MANIFEST_UNKNOWN"),
        (
            format!("/v2/other/runner/manifests/{}", image.digest()),
            "NAME_UNKNOWN",
        ),
        (format!("/v2/{NAME}/blobs/{unknown}"), "BLOB_UNKNOWN"),
    ];

    for (path, code) in cases {
        // when
        let answer = request(&registry, registry.ca_der(), "GET", &path).expect("an answer");

        // then
        assert_eq!(answer.status, 404, "{path}");
        assert_eq!(error_code(&answer), code, "{path}");
    }
}

#[test]
fn should_refuse_the_handshake_when_the_client_trusts_another_ca() {
    // given
    let (image, registry) = published();
    let (_, other) = published();

    // when
    let refused = request(
        &registry,
        other.ca_der(),
        "GET",
        &format!("/v2/{NAME}/manifests/{}", image.digest()),
    );

    // then
    assert!(refused.is_err(), "{refused:?}");
    assert!(registry.served().is_empty());
}

#[test]
fn should_give_the_same_ca_in_pem_and_der_when_asked() {
    // given
    let (_, registry) = published();

    // when
    let from_pem =
        CertificateDer::from_pem_slice(registry.ca_pem().as_bytes()).expect("a PEM certificate");

    // then
    assert_eq!(from_pem.as_ref(), registry.ca_der());
    assert!(
        registry.host().starts_with("127.0.0.1:"),
        "{}",
        registry.host()
    );
}

#[test]
fn should_refuse_connections_when_the_registry_is_stopped() {
    // given
    let (image, registry) = published();
    let addr = registry.addr();
    let ca = registry.ca_der().to_vec();
    let path = format!("/v2/{NAME}/manifests/{}", image.digest());
    assert_eq!(
        request(&registry, &ca, "GET", &path)
            .expect("an answer")
            .status,
        200
    );

    // when
    registry.stop().expect("a clean stop");

    // then
    let refused = std::net::TcpStream::connect(addr);
    assert!(refused.is_err(), "{refused:?}");
}
