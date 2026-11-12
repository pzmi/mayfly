//! The loopback OCI registry `hw01` serves for the `ImageStore` contract's image: plain HTTP on
//! `127.0.0.1` (port 0), for this run only, enough of the distribution API for `tart push
//! --insecure` and `tart pull --insecure` (`/v2/`, blob `HEAD`/`GET` with a byte range, uploads by
//! `POST` then `PATCH`s and a final `PUT`, or one monolithic `PUT`, manifests by tag or digest).
//!
//! Everything lives in files under its directory, read afresh on every request, so the gated
//! contract world (`mayfly-adapter-tart/tests/real_tart_images.rs`) can publish variants and
//! drift by writing them, and read what was asked for:
//!
//! ```text
//! blobs/sha256/<hex>                 a blob
//! manifests/<repository, / as %2F>/<tag or sha256:hex>   a manifest's exact bytes
//! requests.log                       one line per manifest request: <METHOD> <repository>@<ref>
//! uploads/<n>                        an upload in progress
//! ```
//!
//! A manifest is served as stored, with `Docker-Content-Digest` the hash of the bytes served
//! (which is how a drifted manifest is told apart). Pinned by this module's tests.

use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;

use sha2::{Digest, Sha256};

/// The most bytes of a request head or a manifest body read.
const HEAD_LIMIT: usize = 64 << 10;
const MANIFEST_LIMIT: u64 = 4 << 20;

/// The registry, serving until dropped.
#[derive(Debug)]
pub struct Registry {
    /// Where it listens.
    pub addr: SocketAddr,
    /// Its store.
    pub dir: PathBuf,
    stop: Arc<AtomicBool>,
}

/// `sha256:<hex>` of `bytes`.
pub fn digest(bytes: &[u8]) -> String {
    let hash = Sha256::digest(bytes);
    let mut out = String::from("sha256:");
    for b in hash {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// The file a repository's manifest reference is stored in.
pub fn manifest_path(dir: &Path, repository: &str, reference: &str) -> PathBuf {
    dir.join("manifests")
        .join(repository.replace('/', "%2F"))
        .join(reference)
}

/// A repository name the registry stores under: `/`-separated components of `[a-z0-9._-]`,
/// none empty, `.` or `..`, so a name never leaves `manifests/`. Pinned by
/// `tests::should_refuse_a_repository_when_a_component_is_empty_or_a_dot`.
pub fn valid_repository(name: &str) -> bool {
    !name.is_empty()
        && name.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
        })
}

/// The file a blob is stored in, if `digest` is a sha256 digest.
pub fn blob_path(dir: &Path, digest: &str) -> Option<PathBuf> {
    let hex = digest.strip_prefix("sha256:")?;
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| dir.join("blobs").join("sha256").join(hex))
}

impl Registry {
    /// Starts serving `dir` on `127.0.0.1`, port 0.
    ///
    /// # Errors
    /// The directory or the socket could not be made.
    pub fn start(dir: &Path) -> std::io::Result<Self> {
        for sub in ["blobs/sha256", "manifests", "uploads"] {
            fs::create_dir_all(dir.join(sub))?;
        }
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let (stopping, store) = (Arc::clone(&stop), dir.to_path_buf());
        let uploads = Arc::new(AtomicU64::new(0));
        thread::spawn(move || {
            for stream in listener.incoming() {
                if stopping.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(stream) = stream else { continue };
                let (store, uploads) = (store.clone(), Arc::clone(&uploads));
                thread::spawn(move || {
                    let _served = serve(stream, &store, &uploads);
                });
            }
        });
        Ok(Self {
            addr,
            dir: dir.to_path_buf(),
            stop,
        })
    }

    /// `127.0.0.1:<port>`.
    pub fn host(&self) -> String {
        self.addr.to_string()
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the accept loop so it sees the flag.
        let _woken = TcpStream::connect(self.addr);
    }
}

struct Request {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn query(&self, key: &str) -> Option<String> {
        self.query.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == key).then(|| v.replace("%3A", ":").replace("%3a", ":"))
        })
    }
}

fn read_head(reader: &mut impl BufRead) -> std::io::Result<Option<Request>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Err(std::io::Error::other("bad request line"));
    };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut request = Request {
        method: method.to_owned(),
        path: path.to_owned(),
        query: query.to_owned(),
        headers: Vec::new(),
    };
    let mut total = line.len();
    loop {
        line.clear();
        total += reader.read_line(&mut line)?;
        if total > HEAD_LIMIT {
            return Err(std::io::Error::other("head too large"));
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            return Ok(Some(request));
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            request
                .headers
                .push((k.trim().to_owned(), v.trim().to_owned()));
        }
    }
}

/// Copies the request body into `sink`, by `Content-Length` or chunked; returns its length.
fn read_body(
    request: &Request,
    reader: &mut impl BufRead,
    sink: &mut impl Write,
) -> std::io::Result<u64> {
    if request
        .header("Transfer-Encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case("chunked"))
    {
        let mut total = 0;
        loop {
            let mut size = String::new();
            reader.read_line(&mut size)?;
            let size = u64::from_str_radix(size.trim().split(';').next().unwrap_or("0"), 16)
                .map_err(|_| std::io::Error::other("bad chunk size"))?;
            if size == 0 {
                let mut trailer = String::new();
                while reader.read_line(&mut trailer)? > 2 {
                    trailer.clear();
                }
                return Ok(total);
            }
            total += std::io::copy(&mut reader.by_ref().take(size), sink)?;
            let mut crlf = String::new();
            reader.read_line(&mut crlf)?;
        }
    }
    let length: u64 = request
        .header("Content-Length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    std::io::copy(&mut reader.by_ref().take(length), sink)
}

/// A sink that refuses more than [`MANIFEST_LIMIT`] bytes.
struct Capped(Vec<u8>);

impl Write for Capped {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if (self.0.len() + buf.len()) as u64 > MANIFEST_LIMIT {
            return Err(std::io::Error::other("manifest too large"));
        }
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn respond(
    stream: &mut impl Write,
    status: &str,
    headers: &[(&str, String)],
    body: Option<(&mut dyn Read, u64)>,
) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status}\r\n");
    let length = body.as_ref().map_or(0, |(_, n)| *n);
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if !headers.iter().any(|(k, _)| *k == "Content-Length") {
        head.push_str(&format!("Content-Length: {length}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    if let Some((reader, n)) = body {
        std::io::copy(&mut reader.take(n), stream)?;
    }
    stream.flush()
}

fn log(dir: &Path, line: &str) {
    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("requests.log"))
    {
        let _logged = writeln!(file, "{line}");
    }
}

fn serve(stream: TcpStream, dir: &Path, uploads: &AtomicU64) -> std::io::Result<()> {
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    while let Some(request) = read_head(&mut reader)? {
        handle(&request, &mut reader, &mut writer, dir, uploads)?;
        if request
            .header("Connection")
            .is_some_and(|v| v.eq_ignore_ascii_case("close"))
        {
            return Ok(());
        }
    }
    Ok(())
}

/// A body nobody reads still has to leave the connection.
fn discard(request: &Request, reader: &mut impl BufRead) -> std::io::Result<()> {
    read_body(request, reader, &mut std::io::sink()).map(|_| ())
}

fn handle(
    request: &Request,
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    dir: &Path,
    uploads: &AtomicU64,
) -> std::io::Result<()> {
    let api = [("Docker-Distribution-Api-Version", "registry/2.0".to_owned())];
    if request.path == "/v2/" || request.path == "/v2" {
        discard(request, reader)?;
        let mut body: &[u8] = b"{}";
        return respond(writer, "200 OK", &api, Some((&mut body, 2)));
    }
    let Some(rest) = request.path.strip_prefix("/v2/") else {
        discard(request, reader)?;
        return respond(writer, "404 Not Found", &api, None);
    };
    if let Some((repository, reference)) = rest.rsplit_once("/manifests/") {
        if !valid_repository(repository) {
            discard(request, reader)?;
            return respond(writer, "400 Bad Request", &api, None);
        }
        return manifest(request, reader, writer, dir, repository, reference);
    }
    if let Some((repository, upload)) = rest.split_once("/blobs/uploads") {
        if !valid_repository(repository) {
            discard(request, reader)?;
            return respond(writer, "400 Bad Request", &api, None);
        }
        return upload_blob(request, reader, writer, dir, uploads, repository, upload);
    }
    if let Some((_, digest)) = rest.rsplit_once("/blobs/") {
        discard(request, reader)?;
        return blob(request, writer, dir, digest);
    }
    discard(request, reader)?;
    respond(writer, "404 Not Found", &api, None)
}

fn manifest(
    request: &Request,
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    dir: &Path,
    repository: &str,
    reference: &str,
) -> std::io::Result<()> {
    let safe = !reference.contains('/') && !reference.contains("..") && !reference.is_empty();
    if request.method == "PUT" && safe {
        let mut capped = Capped(Vec::new());
        read_body(request, reader, &mut capped)?;
        let bytes = capped.0;
        let digest = digest(&bytes);
        for name in [reference, digest.as_str()] {
            let path = manifest_path(dir, repository, name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, &bytes)?;
        }
        log(dir, &format!("PUT {repository}@{reference}"));
        return respond(
            writer,
            "201 Created",
            &[
                ("Docker-Content-Digest", digest.clone()),
                ("Location", format!("/v2/{repository}/manifests/{digest}")),
            ],
            None,
        );
    }
    discard(request, reader)?;
    log(dir, &format!("{} {repository}@{reference}", request.method));
    let bytes = if safe {
        fs::read(manifest_path(dir, repository, reference)).ok()
    } else {
        None
    };
    let Some(bytes) = bytes else {
        return respond(writer, "404 Not Found", &[], None);
    };
    let media = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| v["mediaType"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "application/vnd.oci.image.manifest.v1+json".to_owned());
    let headers = [
        ("Content-Type", media),
        ("Docker-Content-Digest", digest(&bytes)),
        ("Content-Length", bytes.len().to_string()),
    ];
    let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let mut body = bytes.as_slice();
    let body = (request.method != "HEAD").then_some((&mut body as &mut dyn Read, length));
    respond(writer, "200 OK", &headers, body)
}

/// `bytes=a-` or `bytes=a-b`.
fn range(header: Option<&str>, size: u64) -> Option<(u64, u64)> {
    let spec = header?.strip_prefix("bytes=")?;
    let (start, end) = spec.split_once('-')?;
    let start: u64 = start.parse().ok()?;
    let end: u64 = if end.is_empty() {
        size.checked_sub(1)?
    } else {
        end.parse::<u64>().ok()?.min(size.checked_sub(1)?)
    };
    (start <= end).then_some((start, end))
}

fn blob(
    request: &Request,
    writer: &mut impl Write,
    dir: &Path,
    digest: &str,
) -> std::io::Result<()> {
    let Some(path) = blob_path(dir, digest) else {
        return respond(writer, "404 Not Found", &[], None);
    };
    let Ok(mut file) = fs::File::open(&path) else {
        return respond(writer, "404 Not Found", &[], None);
    };
    let size = file.metadata()?.len();
    let mut headers = vec![
        ("Docker-Content-Digest", digest.to_owned()),
        ("Content-Type", "application/octet-stream".to_owned()),
        ("Accept-Ranges", "bytes".to_owned()),
    ];
    let (status, start, length) = match range(request.header("Range"), size) {
        Some((start, end)) => {
            headers.push(("Content-Range", format!("bytes {start}-{end}/{size}")));
            ("206 Partial Content", start, end - start + 1)
        }
        None => ("200 OK", 0, size),
    };
    if request.method == "HEAD" {
        headers.push(("Content-Length", length.to_string()));
        return respond(writer, status, &headers, None);
    }
    file.seek(SeekFrom::Start(start))?;
    respond(writer, status, &headers, Some((&mut file, length)))
}

fn upload_blob(
    request: &Request,
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    dir: &Path,
    uploads: &AtomicU64,
    repository: &str,
    upload: &str,
) -> std::io::Result<()> {
    let id = upload.trim_start_matches('/');
    if request.method == "POST" && id.is_empty() {
        let id = uploads.fetch_add(1, Ordering::SeqCst).to_string();
        let path = dir.join("uploads").join(&id);
        let mut file = fs::File::create(&path)?;
        read_body(request, reader, &mut file)?;
        if let Some(digest) = request.query("digest") {
            return finish_upload(writer, dir, repository, &path, &digest);
        }
        let location = format!("/v2/{repository}/blobs/uploads/{id}");
        return respond(
            writer,
            "202 Accepted",
            &[
                ("Location", location),
                ("Range", "0-0".to_owned()),
                ("Docker-Upload-UUID", id),
            ],
            None,
        );
    }
    let valid = !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit());
    let path = dir.join("uploads").join(id);
    if !valid || !path.is_file() {
        discard(request, reader)?;
        return respond(writer, "404 Not Found", &[], None);
    }
    let mut file = fs::OpenOptions::new().append(true).open(&path)?;
    read_body(request, reader, &mut file)?;
    drop(file);
    match (request.method.as_str(), request.query("digest")) {
        ("PUT", Some(digest)) => finish_upload(writer, dir, repository, &path, &digest),
        ("PATCH", _) => {
            let size = fs::metadata(&path)?.len();
            respond(
                writer,
                "202 Accepted",
                &[
                    ("Location", format!("/v2/{repository}/blobs/uploads/{id}")),
                    ("Range", format!("0-{}", size.saturating_sub(1))),
                    ("Docker-Upload-UUID", id.to_owned()),
                ],
                None,
            )
        }
        _ => respond(writer, "400 Bad Request", &[], None),
    }
}

fn finish_upload(
    writer: &mut impl Write,
    dir: &Path,
    repository: &str,
    upload: &Path,
    claimed: &str,
) -> std::io::Result<()> {
    let mut hasher = Sha256::new();
    let mut file = fs::File::open(upload)?;
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let mut actual = String::from("sha256:");
    for b in hasher.finalize() {
        actual.push_str(&format!("{b:02x}"));
    }
    match blob_path(dir, claimed) {
        Some(target) if actual == claimed => {
            fs::rename(upload, target)?;
            respond(
                writer,
                "201 Created",
                &[
                    ("Docker-Content-Digest", actual.clone()),
                    ("Location", format!("/v2/{repository}/blobs/{actual}")),
                ],
                None,
            )
        }
        _ => {
            let _removed = fs::remove_file(upload);
            respond(writer, "400 Bad Request", &[], None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(addr: SocketAddr, request: &str, body: &[u8]) -> (String, Vec<u8>) {
        let mut stream = TcpStream::connect(addr).expect("connect");
        stream.write_all(request.as_bytes()).expect("head");
        stream.write_all(body).expect("body");
        let mut reader = BufReader::new(stream);
        let mut status = String::new();
        reader.read_line(&mut status).expect("status");
        let mut length = 0usize;
        let mut location = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("header");
            if line.trim().is_empty() {
                break;
            }
            let lower = line.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("content-length:") {
                length = v.trim().parse().expect("length");
            }
            if lower.starts_with("location:") {
                location = line["location:".len()..].trim().to_owned();
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).expect("body");
        (format!("{} {location}", status.trim()), body)
    }

    #[test]
    fn should_serve_back_what_was_pushed_when_blobs_and_a_manifest_are_uploaded() {
        // given
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = Registry::start(dir.path()).expect("start");
        let blob = b"layer bytes".to_vec();
        let blob_digest = digest(&blob);
        let manifest =
            br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;

        // when: POST, PATCH, PUT for the blob; one PUT for the manifest by tag
        let (started, _) = call(
            registry.addr,
            "POST /v2/mayfly/contract/blobs/uploads/ HTTP/1.1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            b"",
        );
        let location = started
            .split_whitespace()
            .last()
            .expect("location")
            .to_owned();
        let (patched, _) = call(
            registry.addr,
            &format!(
                "PATCH {location} HTTP/1.1\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
            ),
            b"5\r\nlayer\r\n6\r\n bytes\r\n0\r\n\r\n",
        );
        let (put, _) = call(
            registry.addr,
            &format!(
                "PUT {location}?digest={blob_digest} HTTP/1.1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
            b"",
        );
        let (pushed, _) = call(
            registry.addr,
            &format!(
                "PUT /v2/mayfly/contract/manifests/hw01 HTTP/1.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                manifest.len()
            ),
            manifest,
        );
        let (_, served) = call(
            registry.addr,
            &format!(
                "GET /v2/mayfly/contract/manifests/{} HTTP/1.1\r\nConnection: close\r\n\r\n",
                digest(manifest)
            ),
            b"",
        );
        let (ranged, part) = call(
            registry.addr,
            &format!(
                "GET /v2/mayfly/contract/blobs/{blob_digest} HTTP/1.1\r\nRange: bytes=6-\r\nConnection: close\r\n\r\n"
            ),
            b"",
        );

        // then
        assert!(started.starts_with("HTTP/1.1 202"), "{started}");
        assert!(patched.starts_with("HTTP/1.1 202"), "{patched}");
        assert!(put.starts_with("HTTP/1.1 201"), "{put}");
        assert!(pushed.starts_with("HTTP/1.1 201"), "{pushed}");
        assert_eq!(served, manifest);
        assert!(ranged.starts_with("HTTP/1.1 206"), "{ranged}");
        assert_eq!(part, b"bytes");
        let log = fs::read_to_string(dir.path().join("requests.log")).expect("log");
        assert!(
            log.contains(&format!("GET mayfly/contract@{}", digest(manifest))),
            "{log}"
        );
    }

    #[test]
    fn should_refuse_a_repository_when_a_component_is_empty_or_a_dot() {
        // given
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = Registry::start(dir.path()).expect("start");
        let names = [
            "..",
            ".",
            "a/../b",
            "a//b",
            "",
            "A/b",
            "mayfly-contract",
            "mayfly/contract",
        ];

        // when
        let valid: Vec<bool> = names.iter().map(|n| valid_repository(n)).collect();
        let (answer, _) = call(
            registry.addr,
            "PUT /v2/../manifests/x HTTP/1.1\r\nContent-Length: 2\r\nConnection: close\r\n\r\n",
            b"{}",
        );

        // then
        assert_eq!(
            valid,
            [false, false, false, false, false, false, true, true]
        );
        assert!(answer.starts_with("HTTP/1.1 400"), "{answer}");
        assert!(
            !dir.path().join("x").exists(),
            "nothing written outside manifests/"
        );
    }

    #[test]
    fn should_refuse_a_blob_when_its_bytes_do_not_hash_to_the_claimed_digest() {
        // given
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = Registry::start(dir.path()).expect("start");
        let wrong = digest(b"other");

        // when
        let (answer, _) = call(
            registry.addr,
            &format!(
                "POST /v2/r/blobs/uploads/?digest={wrong} HTTP/1.1\r\nContent-Length: 4\r\nConnection: close\r\n\r\n"
            ),
            b"data",
        );

        // then
        assert!(answer.starts_with("HTTP/1.1 400"), "{answer}");
        assert!(!blob_path(dir.path(), &wrong).expect("path").exists());
    }
}
