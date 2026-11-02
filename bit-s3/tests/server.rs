//! The client against a real S3-compatible server, configured the way users
//! configure it: through `AWS_*` variables that [`Config::from_env`] reads.
//! `BIT_S3_TEST_BUCKET` names a bucket on that server to use. `bit s3-test`
//! starts a server in Docker and sets them; without them these tests do
//! nothing.

use std::time::{Duration, Instant};

use bit_s3::{Client, Config, Error};

/// A client and bucket for the configured server, once it accepts requests.
fn server() -> Option<(Client, String)> {
    let bucket = std::env::var("BIT_S3_TEST_BUCKET").ok()?;
    let client = Client::new(Config::from_env().unwrap()).unwrap();
    // A container that has just started may not be listening yet.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match client.head(&bucket, "probe") {
            Ok(_) => return Some((client, bucket)),
            Err(e) if Instant::now() > deadline => panic!("S3 server never became ready: {e}"),
            Err(_) => std::thread::sleep(Duration::from_millis(250)),
        }
    }
}

/// A key no other test or run uses, so runs against a long-lived server
/// do not interfere.
fn unique(name: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("test/{nanos}/{name}")
}

#[test]
fn objects_round_trip() {
    let Some((client, bucket)) = server() else { return };
    let key = unique("actions/receipts.json");

    assert!(!client.head(&bucket, &key).unwrap());
    assert_eq!(client.get(&bucket, &key).unwrap(), None);
    client.put(&bucket, &key, b"[]").unwrap();
    assert!(client.head(&bucket, &key).unwrap());
    assert_eq!(client.get(&bucket, &key).unwrap().as_deref(), Some(&b"[]"[..]));
}

#[test]
fn files_round_trip() {
    let Some((client, bucket)) = server() else { return };
    let key = unique("cas/blob");
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("blob");
    let content: Vec<u8> = (0..4 << 20).map(|i| (i % 251) as u8).collect();
    std::fs::write(&src, &content).unwrap();

    client.put_file(&bucket, &key, &src).unwrap();
    let dest = dir.path().join("copy");
    assert!(client.get_to_file(&bucket, &key, &dest).unwrap());
    assert_eq!(std::fs::read(&dest).unwrap(), content);
}

/// Proves the server checks signatures, so the round trips above prove
/// ours are right.
#[test]
fn a_wrong_secret_is_rejected() {
    let Some((_, bucket)) = server() else { return };
    let mut config = Config::from_env().unwrap();
    config.credentials.secret_access_key.push_str("-wrong");
    let err = Client::new(config)
        .unwrap()
        .put(&bucket, &unique("denied"), b"x")
        .unwrap_err();
    assert!(matches!(err, Error::Status { status: 403, .. }), "{err}");
}
