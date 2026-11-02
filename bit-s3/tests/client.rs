//! The client against a minimal in-process S3 stand-in, which stores objects
//! by path and records every request it receives.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use bit_s3::{Client, Config, Credentials, Error};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    path: String,
    headers: HashMap<String, String>,
}

#[derive(Default)]
struct State {
    objects: HashMap<String, Vec<u8>>,
    requests: Vec<Recorded>,
}

struct Server {
    endpoint: String,
    state: Arc<Mutex<State>>,
}

impl Server {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State::default()));
        let shared = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = shared.clone();
                std::thread::spawn(move || serve(stream, &state));
            }
        });
        Self { endpoint, state }
    }

    fn client(&self) -> Client {
        Client::new(Config {
            credentials: Credentials {
                access_key_id: "id".into(),
                secret_access_key: "secret".into(),
                session_token: None,
            },
            region: "us-east-1".into(),
            endpoint: Some(format!("{}/base/", self.endpoint)),
        })
        .unwrap()
    }

    fn requests(&self) -> Vec<Recorded> {
        self.state.lock().unwrap().requests.clone()
    }
}

/// Answer requests on one connection until the client closes it.
fn serve(stream: TcpStream, state: &Mutex<State>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = stream;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap().to_owned();
        let path = parts.next().unwrap().to_owned();
        let mut headers = HashMap::new();
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            let (name, value) = header.split_once(':').unwrap();
            headers.insert(name.to_lowercase(), value.trim().to_owned());
        }
        let length: usize = headers.get("content-length").map_or(0, |v| v.parse().unwrap());
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();

        let mut state = state.lock().unwrap();
        state.requests.push(Recorded {
            method: method.clone(),
            path: path.clone(),
            headers,
        });
        let (status, response): (&str, Vec<u8>) = if path.contains("forbidden") {
            (
                "403 Forbidden",
                b"<?xml version=\"1.0\"?><Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>"
                    .to_vec(),
            )
        } else {
            match (method.as_str(), state.objects.get(&path)) {
                ("PUT", _) => {
                    state.objects.insert(path, body);
                    ("200 OK", Vec::new())
                }
                ("GET" | "HEAD", Some(object)) => ("200 OK", object.clone()),
                _ => ("404 Not Found", b"<Error><Code>NoSuchKey</Code></Error>".to_vec()),
            }
        };
        drop(state);
        let mut head = format!("HTTP/1.1 {status}\r\ncontent-length: {}\r\n\r\n", response.len()).into_bytes();
        if method != "HEAD" {
            head.extend(response);
        }
        writer.write_all(&head).unwrap();
    }
}

#[test]
fn objects_round_trip_by_path_under_the_endpoint() {
    let server = Server::start();
    let client = server.client();

    assert!(!client.head("cache", "actions/k.json").unwrap());
    assert_eq!(client.get("cache", "actions/k.json").unwrap(), None);
    client.put("cache", "actions/k.json", b"receipts").unwrap();
    assert!(client.head("cache", "actions/k.json").unwrap());
    assert_eq!(
        client.get("cache", "actions/k.json").unwrap().as_deref(),
        Some(&b"receipts"[..])
    );

    let put = server.requests().into_iter().find(|r| r.method == "PUT").unwrap();
    assert_eq!(put.path, "/base/cache/actions/k.json");
    assert_eq!(
        put.headers["x-amz-content-sha256"],
        format!("{:x}", Sha256::digest(b"receipts")),
        "the payload is signed"
    );
    let authorization = &put.headers["authorization"];
    assert!(
        authorization.starts_with("AWS4-HMAC-SHA256 Credential=id/")
            && authorization.contains("/us-east-1/s3/aws4_request"),
        "{authorization}"
    );
    assert!(put.headers.contains_key("x-amz-date"));
}

#[test]
fn files_stream_both_ways() {
    let server = Server::start();
    let client = server.client();
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("blob");
    let content = vec![7u8; 1 << 20];
    std::fs::write(&src, &content).unwrap();

    client.put_file("cache", "cas/ab/abcd", &src).unwrap();
    let dest = dir.path().join("copy");
    assert!(client.get_to_file("cache", "cas/ab/abcd", &dest).unwrap());
    assert_eq!(std::fs::read(&dest).unwrap(), content);
    assert!(
        !client
            .get_to_file("cache", "cas/ab/missing", &dir.path().join("none"))
            .unwrap()
    );
    assert!(!dir.path().join("none").exists(), "a missing object creates no file");
}

#[test]
fn errors_carry_the_message_s3_sent() {
    let server = Server::start();
    let err = server.client().get("cache", "forbidden").unwrap_err();
    assert!(
        matches!(&err, Error::Status { status: 403, message, .. } if message == "Access Denied"),
        "{err}"
    );
}

#[test]
fn an_invalid_endpoint_is_rejected() {
    let err = Client::new(Config {
        credentials: Credentials {
            access_key_id: "id".into(),
            secret_access_key: "secret".into(),
            session_token: None,
        },
        region: "us-east-1".into(),
        endpoint: Some("localhost".into()),
    })
    .err()
    .unwrap();
    assert!(matches!(err, Error::InvalidEndpoint { .. }), "{err}");
}
