use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

/// Where the build cache lives, as `--cache` or `BIT_CACHE` names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// The cache on local disk: `disk://` for the platform's cache
    /// directory, or `disk://<path>` for another.
    Disk(Option<PathBuf>),
    /// An S3 bucket behind the default disk cache: `s3://bucket/prefix`,
    /// read-only, or `s3+rw://bucket/prefix`, which results are also
    /// published to.
    S3 {
        bucket: String,
        /// Key prefix without leading or trailing slashes, possibly empty.
        prefix: String,
        writable: bool,
    },
}

impl Default for Location {
    fn default() -> Self {
        Self::Disk(None)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LocationError {
    #[error("`{0}` is not a cache URI: expected disk://, s3://, or s3+rw://")]
    UnknownScheme(String),
    #[error("`{0}` names no bucket")]
    NoBucket(String),
}

impl FromStr for Location {
    type Err = LocationError;

    fn from_str(uri: &str) -> Result<Self, Self::Err> {
        let Some((scheme, rest)) = uri.split_once("://") else {
            return Err(LocationError::UnknownScheme(uri.to_owned()));
        };
        let writable = match scheme {
            "disk" => return Ok(Self::Disk((!rest.is_empty()).then(|| PathBuf::from(rest)))),
            "s3" => false,
            "s3+rw" => true,
            _ => return Err(LocationError::UnknownScheme(uri.to_owned())),
        };
        let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
        if bucket.is_empty() {
            return Err(LocationError::NoBucket(uri.to_owned()));
        }
        Ok(Self::S3 {
            bucket: bucket.to_owned(),
            prefix: prefix.trim_matches('/').to_owned(),
            writable,
        })
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disk(None) => write!(f, "disk://"),
            Self::Disk(Some(path)) => write!(f, "disk://{}", path.display()),
            Self::S3 {
                bucket,
                prefix,
                writable,
            } => {
                let scheme = if *writable { "s3+rw" } else { "s3" };
                write!(f, "{scheme}://{bucket}")?;
                if prefix.is_empty() {
                    Ok(())
                } else {
                    write!(f, "/{prefix}")
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s3(bucket: &str, prefix: &str, writable: bool) -> Location {
        Location::S3 {
            bucket: bucket.into(),
            prefix: prefix.into(),
            writable,
        }
    }

    #[test]
    fn parses_every_scheme() {
        assert_eq!("disk://".parse(), Ok(Location::Disk(None)));
        assert_eq!(
            "disk:///var/cache/bit".parse(),
            Ok(Location::Disk(Some("/var/cache/bit".into())))
        );
        assert_eq!("disk://cache".parse(), Ok(Location::Disk(Some("cache".into()))));
        assert_eq!("s3://bucket".parse(), Ok(s3("bucket", "", false)));
        assert_eq!("s3://bucket/team/bit/".parse(), Ok(s3("bucket", "team/bit", false)));
        assert_eq!("s3+rw://bucket/ci".parse(), Ok(s3("bucket", "ci", true)));
    }

    #[test]
    fn rejects_what_it_cannot_use() {
        assert!(matches!(
            "gs://bucket".parse::<Location>(),
            Err(LocationError::UnknownScheme(_))
        ));
        assert!(matches!(
            "/var/cache".parse::<Location>(),
            Err(LocationError::UnknownScheme(_))
        ));
        assert!(matches!(
            "s3:///prefix".parse::<Location>(),
            Err(LocationError::NoBucket(_))
        ));
    }

    #[test]
    fn displays_as_parsed() {
        for uri in ["disk://", "disk:///tmp/c", "s3://b", "s3+rw://b/p/q"] {
            assert_eq!(uri.parse::<Location>().unwrap().to_string(), uri);
        }
    }
}
