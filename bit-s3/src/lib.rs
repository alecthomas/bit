//! A small, synchronous S3 client: single-object GET, PUT, and HEAD, signed
//! with AWS Signature Version 4, against AWS or any S3-compatible service.
//!
//! [`Config::from_env`] finds credentials, region, and endpoint the way the
//! AWS CLI does, for the common cases.

mod config;
mod sigv4;

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};
use ureq::http::{Response, Uri};
use ureq::{Agent, Body};

pub use config::{Config, Credentials};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "no AWS credentials found: set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY, or configure the `{profile}` profile"
    )]
    NoCredentials { profile: String },
    #[error(
        "AWS profile `{profile}` uses `{setting}`, which is not supported directly; \
         add `credential_process = aws configure export-credentials --profile {profile} --format process` \
         to another profile and use that one"
    )]
    UnsupportedProfile { profile: String, setting: String },
    #[error("credential_process `{command}` failed: {reason}")]
    CredentialProcess { command: String, reason: String },
    #[error("cannot read {}: {source}", path.display())]
    ConfigFile { path: PathBuf, source: io::Error },
    #[error("invalid S3 endpoint `{endpoint}`: {reason}")]
    InvalidEndpoint { endpoint: String, reason: String },
    #[error("{method} {url}: {source}")]
    Http {
        method: &'static str,
        url: String,
        source: Box<ureq::Error>,
    },
    #[error("{method} {url}: HTTP {status}: {message}")]
    Status {
        method: &'static str,
        url: String,
        status: u16,
        message: String,
    },
    #[error("{} {}: {source}", if *download { "downloading to" } else { "uploading" }, path.display())]
    Transfer {
        download: bool,
        path: PathBuf,
        source: io::Error,
    },
}

/// A client for one region and endpoint. Cheap to share between threads:
/// requests reuse pooled connections.
pub struct Client {
    agent: Agent,
    config: Config,
    endpoint: Option<Endpoint>,
}

/// A custom endpoint, split into the parts a request URL is built from.
struct Endpoint {
    scheme: String,
    authority: String,
    /// Path prefix without a trailing slash, often empty.
    base_path: String,
}

impl Endpoint {
    fn parse(endpoint: &str) -> Result<Self, Error> {
        let invalid = |reason: &str| Error::InvalidEndpoint {
            endpoint: endpoint.to_owned(),
            reason: reason.to_owned(),
        };
        let uri: Uri = endpoint
            .parse()
            .map_err(|e: ureq::http::uri::InvalidUri| invalid(&e.to_string()))?;
        let scheme = uri.scheme_str().ok_or_else(|| invalid("missing scheme"))?;
        let authority = uri.authority().ok_or_else(|| invalid("missing host"))?;
        Ok(Self {
            scheme: scheme.to_owned(),
            authority: authority.as_str().to_owned(),
            base_path: uri.path().trim_end_matches('/').to_owned(),
        })
    }
}

/// Where one object is: the URL to request, and the host and path to sign.
struct Location {
    url: String,
    host: String,
    path: String,
}

impl Client {
    pub fn new(config: Config) -> Result<Self, Error> {
        let endpoint = config.endpoint.as_deref().map(Endpoint::parse).transpose()?;
        let agent = Agent::config_builder()
            // Statuses are S3's answers, such as 404 for a missing object,
            // not transport failures.
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(10)))
            .build()
            .into();
        Ok(Self {
            agent,
            config,
            endpoint,
        })
    }

    /// Whether the object exists.
    pub fn head(&self, bucket: &str, key: &str) -> Result<bool, Error> {
        let location = self.locate(bucket, key);
        let mut request = self.agent.head(&location.url);
        for (name, value) in self.sign("HEAD", &location, sigv4::EMPTY_PAYLOAD) {
            request = request.header(name, value);
        }
        let response = request.call().map_err(|e| http_error("HEAD", &location, e))?;
        Ok(found("HEAD", &location, response)?.is_some())
    }

    /// The object's content, or `None` if there is no such object. Meant
    /// for small objects; see [`Client::get_to_file`] for large ones.
    pub fn get(&self, bucket: &str, key: &str) -> Result<Option<Vec<u8>>, Error> {
        let Some(mut response) = self.get_response(bucket, key)? else {
            return Ok(None);
        };
        let location = self.locate(bucket, key);
        let bytes = response
            .body_mut()
            .with_config()
            .limit(u64::MAX)
            .read_to_vec()
            .map_err(|e| http_error("GET", &location, e))?;
        Ok(Some(bytes))
    }

    /// Stream the object to `dest`, returning `false` if there is no such
    /// object. `dest` may be left partly written if the transfer fails.
    pub fn get_to_file(&self, bucket: &str, key: &str, dest: &Path) -> Result<bool, Error> {
        let Some(mut response) = self.get_response(bucket, key)? else {
            return Ok(false);
        };
        let transfer = |source| Error::Transfer {
            download: true,
            path: dest.to_owned(),
            source,
        };
        let mut file = File::create(dest).map_err(transfer)?;
        io::copy(&mut response.body_mut().as_reader(), &mut file).map_err(transfer)?;
        Ok(true)
    }

    fn get_response(&self, bucket: &str, key: &str) -> Result<Option<Response<Body>>, Error> {
        let location = self.locate(bucket, key);
        let mut request = self.agent.get(&location.url);
        for (name, value) in self.sign("GET", &location, sigv4::EMPTY_PAYLOAD) {
            request = request.header(name, value);
        }
        let response = request.call().map_err(|e| http_error("GET", &location, e))?;
        found("GET", &location, response)
    }

    pub fn put(&self, bucket: &str, key: &str, body: &[u8]) -> Result<(), Error> {
        let location = self.locate(bucket, key);
        let payload = sigv4::hex(&Sha256::digest(body));
        let mut request = self.agent.put(&location.url);
        for (name, value) in self.sign("PUT", &location, &payload) {
            request = request.header(name, value);
        }
        let response = request.send(body).map_err(|e| http_error("PUT", &location, e))?;
        succeeded("PUT", &location, response)
    }

    /// Upload the file at `src`. It is read twice: once to hash it for the
    /// signature, which makes S3 reject a body that changed in transit,
    /// and once to send it.
    pub fn put_file(&self, bucket: &str, key: &str, src: &Path) -> Result<(), Error> {
        let transfer = |source| Error::Transfer {
            download: false,
            path: src.to_owned(),
            source,
        };
        let mut file = File::open(src).map_err(transfer)?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0; 64 * 1024];
        loop {
            let read = file.read(&mut buffer).map_err(transfer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        let payload = sigv4::hex(&hasher.finalize());

        let location = self.locate(bucket, key);
        let mut request = self.agent.put(&location.url);
        for (name, value) in self.sign("PUT", &location, &payload) {
            request = request.header(name, value);
        }
        let file = File::open(src).map_err(transfer)?;
        let response = request.send(file).map_err(|e| http_error("PUT", &location, e))?;
        succeeded("PUT", &location, response)
    }

    /// Where `key` in `bucket` is. A custom endpoint is addressed by path,
    /// `<endpoint>/<bucket>/<key>`, which every S3-compatible service
    /// supports. AWS is addressed by virtual host, except for a bucket whose
    /// name contains a dot, which the wildcard TLS certificate cannot cover.
    fn locate(&self, bucket: &str, key: &str) -> Location {
        let key = key.trim_start_matches('/');
        let (scheme, host, path) = match &self.endpoint {
            Some(endpoint) => (
                endpoint.scheme.as_str(),
                endpoint.authority.clone(),
                format!("{}/{bucket}/{key}", endpoint.base_path),
            ),
            None if bucket.contains('.') => (
                "https",
                format!("s3.{}.amazonaws.com", self.config.region),
                format!("/{bucket}/{key}"),
            ),
            None => (
                "https",
                format!("{bucket}.s3.{}.amazonaws.com", self.config.region),
                format!("/{key}"),
            ),
        };
        let path = sigv4::encode_path(&path);
        Location {
            url: format!("{scheme}://{host}{path}"),
            host,
            path,
        }
    }

    fn sign(&self, method: &str, location: &Location, payload_sha256: &str) -> Vec<(String, String)> {
        sigv4::sign(
            sigv4::Request {
                method,
                host: &location.host,
                path: &location.path,
                payload_sha256,
                headers: Vec::new(),
            },
            &self.config.credentials,
            &self.config.region,
            &sigv4::Timestamp::now(),
        )
    }
}

fn http_error(method: &'static str, location: &Location, source: ureq::Error) -> Error {
    Error::Http {
        method,
        url: location.url.clone(),
        source: Box::new(source),
    }
}

/// The response if it holds the object, `None` if there is no such object,
/// or the error S3 reported.
fn found(method: &'static str, location: &Location, response: Response<Body>) -> Result<Option<Response<Body>>, Error> {
    match response.status().as_u16() {
        200..=299 => Ok(Some(response)),
        404 => Ok(None),
        _ => Err(status_error(method, location, response)),
    }
}

fn succeeded(method: &'static str, location: &Location, response: Response<Body>) -> Result<(), Error> {
    if response.status().is_success() {
        Ok(())
    } else {
        Err(status_error(method, location, response))
    }
}

/// The error for a failed request, with the message from S3's XML error
/// body when there is one.
fn status_error(method: &'static str, location: &Location, mut response: Response<Body>) -> Error {
    let status = response.status().as_u16();
    let body = response.body_mut().read_to_string().unwrap_or_default();
    let element = |name: &str| {
        let start = body.find(&format!("<{name}>"))? + name.len() + 2;
        let end = body[start..].find(&format!("</{name}>"))? + start;
        Some(body[start..end].to_owned())
    };
    let message = element("Message")
        .or_else(|| element("Code"))
        .unwrap_or_else(|| response.status().canonical_reason().unwrap_or("").to_owned());
    Error::Status {
        method,
        url: location.url.clone(),
        status,
        message,
    }
}
