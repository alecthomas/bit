//! AWS Signature Version 4, as S3 uses it for header-signed requests.

use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

use crate::Credentials;

/// The hash of an empty payload, which every request without a body signs.
pub(crate) const EMPTY_PAYLOAD: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// A request time in the two forms a signature uses.
pub(crate) struct Timestamp {
    /// `YYYYMMDD'T'HHMMSS'Z'`, sent as `x-amz-date`.
    amz_date: String,
    /// `YYYYMMDD`, part of the credential scope.
    date: String,
}

impl Timestamp {
    pub(crate) fn now() -> Self {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        Self::from_unix(secs)
    }

    fn from_unix(secs: u64) -> Self {
        let days = (secs / 86_400) as i64;
        let rem = secs % 86_400;
        let (year, month, day) = civil_from_days(days);
        let date = format!("{year:04}{month:02}{day:02}");
        let amz_date = format!("{date}T{:02}{:02}{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60);
        Self { amz_date, date }
    }
}

/// The Gregorian date `days` after 1970-01-01, by Howard Hinnant's
/// `civil_from_days` algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// One request to sign. `path` must already be URI-encoded, as
/// [`encode_path`] does, and S3 signs it exactly as sent.
pub(crate) struct Request<'a> {
    pub method: &'a str,
    pub host: &'a str,
    pub path: &'a str,
    /// Hex SHA-256 of the body, sent as `x-amz-content-sha256`.
    pub payload_sha256: &'a str,
    /// Further headers to sign, with lowercase names.
    pub headers: Vec<(String, String)>,
}

/// Sign `request`, returning every header to send with it, `authorization`
/// among them. `host` is signed but not returned, because the HTTP client
/// sets it from the URL.
pub(crate) fn sign(
    request: Request<'_>,
    credentials: &Credentials,
    region: &str,
    time: &Timestamp,
) -> Vec<(String, String)> {
    let mut headers = request.headers;
    headers.push(("host".into(), request.host.into()));
    headers.push(("x-amz-content-sha256".into(), request.payload_sha256.into()));
    headers.push(("x-amz-date".into(), time.amz_date.clone()));
    if let Some(token) = &credentials.session_token {
        headers.push(("x-amz-security-token".into(), token.clone()));
    }
    headers.sort();

    let canonical_headers: String = headers
        .iter()
        .map(|(name, value)| format!("{name}:{}\n", value.trim()))
        .collect();
    let signed_headers = headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_request = format!(
        "{}\n{}\n\n{canonical_headers}\n{signed_headers}\n{}",
        request.method, request.path, request.payload_sha256
    );

    let scope = format!("{}/{region}/s3/aws4_request", time.date);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
        time.amz_date,
        hex(&Sha256::digest(canonical_request.as_bytes()))
    );
    let key = [time.date.as_str(), region, "s3", "aws4_request"].iter().fold(
        format!("AWS4{}", credentials.secret_access_key).into_bytes(),
        |key, part| hmac(&key, part.as_bytes()),
    );
    let signature = hex(&hmac(&key, string_to_sign.as_bytes()));

    headers.retain(|(name, _)| name != "host");
    headers.push((
        "authorization".into(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            credentials.access_key_id
        ),
    ));
    headers
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    // HMAC accepts a key of any length, so this cannot fail.
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// URI-encode an object path as S3 signs it: every byte except unreserved
/// characters and `/` as `%XX`.
pub(crate) fn encode_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_credentials() -> Credentials {
        Credentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        }
    }

    /// The "GET Object" example from AWS's S3 documentation, "Signature
    /// Calculations for the Authorization Header: Transferring Payload in a
    /// Single Chunk".
    #[test]
    fn matches_the_documented_get_object_example() {
        let time = Timestamp::from_unix(1_369_353_600);
        assert_eq!(time.amz_date, "20130524T000000Z");
        let headers = sign(
            Request {
                method: "GET",
                host: "examplebucket.s3.amazonaws.com",
                path: "/test.txt",
                payload_sha256: EMPTY_PAYLOAD,
                headers: vec![("range".into(), "bytes=0-9".into())],
            },
            &example_credentials(),
            "us-east-1",
            &time,
        );
        let authorization = &headers.iter().find(|(name, _)| name == "authorization").unwrap().1;
        assert_eq!(
            authorization,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
        assert!(!headers.iter().any(|(name, _)| name == "host"));
    }

    #[test]
    fn a_session_token_is_sent_and_signed() {
        let credentials = Credentials {
            session_token: Some("token".into()),
            ..example_credentials()
        };
        let headers = sign(
            Request {
                method: "HEAD",
                host: "h",
                path: "/k",
                payload_sha256: EMPTY_PAYLOAD,
                headers: vec![],
            },
            &credentials,
            "us-east-1",
            &Timestamp::now(),
        );
        assert!(headers.contains(&("x-amz-security-token".into(), "token".into())));
        let authorization = &headers.iter().find(|(name, _)| name == "authorization").unwrap().1;
        assert!(authorization.contains("x-amz-security-token"), "{authorization}");
    }

    #[test]
    fn dates_are_computed_in_utc() {
        assert_eq!(Timestamp::from_unix(0).amz_date, "19700101T000000Z");
        assert_eq!(Timestamp::from_unix(951_782_400).date, "20000229");
        assert_eq!(Timestamp::from_unix(1_798_761_599).amz_date, "20261231T235959Z");
    }

    #[test]
    fn paths_keep_slashes_and_unreserved_characters() {
        assert_eq!(encode_path("/a/b-c_d.e~f"), "/a/b-c_d.e~f");
        assert_eq!(encode_path("/a b/c+d=é"), "/a%20b/c%2Bd%3D%C3%A9");
    }
}
