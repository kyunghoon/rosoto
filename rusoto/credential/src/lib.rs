#![cfg_attr(feature = "nightly-testing", feature(plugin))]
#![cfg_attr(feature = "nightly-testing", plugin(clippy))]
#![cfg_attr(not(feature = "unstable"), deny(warnings))]

//! Types for loading and managing AWS access credentials for API requests.

extern crate chrono;
extern crate reqwest;
extern crate regex;
extern crate serde_json;

pub use environment::EnvironmentProvider;
pub use container::ContainerProvider;
pub use static_provider::StaticProvider;
pub use instance_metadata::InstanceMetadataProvider;
pub use profile::ProfileProvider;

mod container;
mod environment;
mod static_provider;
mod instance_metadata;
mod profile;
pub mod claims;

use std::fmt;
use std::error::Error;
use std::io::Error as IoError;
use std::sync::Mutex;
use std::cell::RefCell;
use std::collections::BTreeMap;

use chrono::{Duration, Utc, DateTime, ParseError};
use serde_json::{from_str as json_from_str, Value};

/// AWS API access credentials, including access key, secret key, token (for IAM profiles),
/// expiration timestamp, and claims from federated login.
#[derive(Clone, Debug)]
pub struct AwsCredentials {
    key: String,
    secret: String,
    token: Option<String>,
    expires_at: DateTime<Utc>,
    claims: BTreeMap<String, String>,
}

impl AwsCredentials {
    /// Create a new `AwsCredentials` from a key ID, secret key, optional access token, and expiry
    /// time.
    pub fn new<K, S>(key:K, secret:S, token:Option<String>, expires_at:DateTime<Utc>)
    -> AwsCredentials where K:Into<String>, S:Into<String> {
        AwsCredentials {
            key: key.into(),
            secret: secret.into(),
            token: token,
            expires_at: expires_at,
            claims: BTreeMap::new(),
        }
    }

    /// Get a reference to the access key ID.
    pub fn aws_access_key_id(&self) -> &str {
        &self.key
    }

    /// Get a reference to the secret access key.
    pub fn aws_secret_access_key(&self) -> &str {
        &self.secret
    }

    /// Get a reference to the expiry time.
    pub fn expires_at(&self) -> &DateTime<Utc> {
        &self.expires_at
    }

    /// Get a reference to the access token.
    pub fn token(&self) -> &Option<String> {
        &self.token
    }

    /// Determine whether or not the credentials are expired.
    fn credentials_are_expired(&self) -> bool {
        // This is a rough hack to hopefully avoid someone requesting creds then sitting on them
        // before issuing the request:
        self.expires_at < Utc::now() + Duration::seconds(20)
    }

    /// Get the token claims
    pub fn claims(&self) -> &BTreeMap<String, String> {
        &self.claims
    }

    /// Get the mutable token claims
    pub fn claims_mut(&mut self) -> &mut BTreeMap<String, String> {
        &mut self.claims
    }
}

#[derive(Debug, PartialEq)]
pub struct CredentialsError {
    pub message: String
}

impl CredentialsError {
    pub fn new<S>(message: S) -> CredentialsError where S: Into<String>  {
        CredentialsError {
            message: message.into()
        }
    }
}

impl fmt::Display for CredentialsError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.description())
    }
}

impl Error for CredentialsError {
    fn description(&self) -> &str {
        &self.message
    }
}

impl From<ParseError> for CredentialsError {
    fn from(err: ParseError) -> CredentialsError {
        CredentialsError::new(err.description())
    }
}

impl From<IoError> for CredentialsError {
    fn from(err: IoError) -> CredentialsError {
        CredentialsError::new(err.description())
    }
}

/// A trait for types that produce `AwsCredentials`.
pub trait ProvideAwsCredentials {
    /// Produce a new `AwsCredentials`.
    fn credentials(&self) -> Result<AwsCredentials, CredentialsError>;
}

/// Wrapper for `ProvideAwsCredentials` that caches the credentials returned by the
/// wrapped provider.  Each time the credentials are accessed, they are checked to see if
/// they have expired, in which case they are retrieved from the wrapped provider again.
#[derive(Debug)]
pub struct BaseAutoRefreshingProvider<P, T> {
    credentials_provider: P,
    cached_credentials: T
}

/// Threadsafe `AutoRefreshingProvider` that locks cached credentials with a `Mutex`
pub type AutoRefreshingProviderSync<P> = BaseAutoRefreshingProvider<P, Mutex<AwsCredentials>>;

impl <P: ProvideAwsCredentials> AutoRefreshingProviderSync<P> {
    pub fn with_mutex(provider: P) -> Result<AutoRefreshingProviderSync<P>, CredentialsError> {
        let creds = try!(provider.credentials());
        Ok(BaseAutoRefreshingProvider {
            credentials_provider: provider,
            cached_credentials: Mutex::new(creds)
        })
    }
}

impl <P: ProvideAwsCredentials> ProvideAwsCredentials for BaseAutoRefreshingProvider<P, Mutex<AwsCredentials>> {
    fn credentials(&self) -> Result<AwsCredentials, CredentialsError> {
        let mut creds = self.cached_credentials.lock().expect("Failed to lock the cached credentials Mutex");
        if creds.credentials_are_expired() {
            *creds = try!(self.credentials_provider.credentials());
        }
        Ok(creds.clone())
    }
}

/// `!Sync` `AutoRefreshingProvider` that caches credentials in a `RefCell`
pub type AutoRefreshingProvider<P> = BaseAutoRefreshingProvider<P, RefCell<AwsCredentials>>;

impl <P: ProvideAwsCredentials> AutoRefreshingProvider<P> {
    pub fn with_refcell(provider: P) -> Result<AutoRefreshingProvider<P>, CredentialsError> {
        let creds = try!(provider.credentials());
        Ok(BaseAutoRefreshingProvider {
            credentials_provider: provider,
            cached_credentials: RefCell::new(creds)
        })
    }
}

impl <P: ProvideAwsCredentials> ProvideAwsCredentials for BaseAutoRefreshingProvider<P, RefCell<AwsCredentials>> {
    fn credentials(&self) -> Result<AwsCredentials, CredentialsError> {

        let mut creds = self.cached_credentials.borrow_mut();

        if creds.credentials_are_expired() {
            *creds = try!(self.credentials_provider.credentials());
        }

        Ok(creds.clone())
    }
}

/// The credentials provider you probably want to use if you don't require Sync for your AWS services.
/// Wraps a `ChainProvider` in an `AutoRefreshingProvider` that uses a `RefCell` to cache credentials
///
/// The underlying `ChainProvider` checks multiple sources for credentials, and the `AutoRefreshingProvider`
/// refreshes the credentials automatically when they expire.  The `RefCell` allows this caching to happen
/// without the overhead of a `Mutex`, but is `!Sync`.
///
/// For a `Sync` implementation of the same, see `DefaultCredentialsProviderSync`
pub type DefaultCredentialsProvider = AutoRefreshingProvider<ChainProvider>;

impl DefaultCredentialsProvider {
    pub fn new() -> Result<DefaultCredentialsProvider, CredentialsError> {
        Ok(try!(AutoRefreshingProvider::with_refcell(ChainProvider::new())))
    }
}

/// The credentials provider you probably want to use if you do require Sync for your AWS services.
/// Wraps a `ChainProvider` in an `AutoRefreshingProvider` that uses a `Mutex` to lock credentials in a
/// threadsafe manner.
///
/// The underlying `ChainProvider` checks multiple sources for credentials, and the `AutoRefreshingProvider`
/// refreshes the credentials automatically when they expire.  The `Mutex` allows this caching to happen
/// in a Sync manner, incurring the overhead of a Mutex when credentials expire and need to be refreshed.
///
/// For a `!Sync` implementation of the same, see `DefaultCredentialsProvider`
pub type DefaultCredentialsProviderSync = AutoRefreshingProviderSync<ChainProvider>;

impl DefaultCredentialsProviderSync {
    pub fn new() -> Result<DefaultCredentialsProviderSync, CredentialsError> {
        Ok(try!(AutoRefreshingProviderSync::with_mutex(ChainProvider::new())))
    }
}

/// Provides AWS credentials from multiple possible sources using a priority order.
///
/// The following sources are checked in order for credentials when calling `credentials`:
///
/// 1. Environment variables: `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`
/// 2. AWS credentials file. Usually located at `~/.aws/credentials`.
/// 3. IAM instance profile. Will only work if running on an EC2 instance with an instance profile/role.
///
/// If the sources are exhausted without finding credentials, an error is returned.
#[derive(Debug, Default, Clone)]
pub struct ChainProvider {
    profile_provider: Option<ProfileProvider>,
}

impl ProvideAwsCredentials for ChainProvider {
    fn credentials(&self) -> Result<AwsCredentials, CredentialsError> {

    EnvironmentProvider.credentials()
        .or_else(|_| {
            match self.profile_provider {
                Some(ref provider) => provider.credentials(),
                None => Err(CredentialsError::new(""))
            }
        })
        .or_else(|_| ContainerProvider.credentials())
        .or_else(|_| InstanceMetadataProvider.credentials())
        .or_else(|_| Err(CredentialsError::new("Couldn't find AWS credentials in environment, credentials file, or IAM role.")))
    }
}

impl ChainProvider {
    /// Create a new `ChainProvider` using a `ProfileProvider` with the default settings.
    pub fn new() -> ChainProvider {
        ChainProvider {
            profile_provider: ProfileProvider::new().ok(),
        }
    }

    /// Create a new `ChainProvider` using the provided `ProfileProvider`.
    pub fn with_profile_provider(profile_provider: ProfileProvider)
    -> ChainProvider {
        ChainProvider {
            profile_provider: Some(profile_provider),
        }
    }
}

/// Gets the DateTime that is 10 minutes from the current Time.
fn in_ten_minutes() -> DateTime<Utc> {
    Utc::now() + Duration::seconds(600)
}

/// Reduces Boilerplate on getting json values. Wraps `serde_json::Value.get(key)`.
fn extract_string_value_from_json(json_object: &Value, key: &str) -> Result<String, CredentialsError> {
    match json_object.get(key) {
        Some(v) => Ok(v.as_str().expect(&format!("{} value was not a string", key)).to_owned()),
        None => Err(CredentialsError::new(format!("Couldn't find {} in response.", key))),
    }
}

/// Parses the response from an AWS Metadata Service, either from an IAM Role, or a Container.
fn parse_credentials_from_aws_service(response: &str) -> Result<AwsCredentials, CredentialsError> {
    let json_object: Value = match json_from_str(response) {
        Ok(v) => v,
        Err(_) => return Err(CredentialsError::new("Couldn't parse credentials response body.")),
    };

    let access_key_id = try!(extract_string_value_from_json(&json_object, "AccessKeyId"));
    let secret_access_key = try!(extract_string_value_from_json(&json_object, "SecretAccessKey"));
    let token = try!(extract_string_value_from_json(&json_object, "Token"));
    let expiration = try!(extract_string_value_from_json(&json_object, "Expiration"));

    let expiration = try!(expiration.parse());

    Ok(AwsCredentials::new(access_key_id, secret_access_key, Some(token), expiration))
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::fs::{self, File};
    use std::path::Path;

    use super::*;

    #[test]
    fn credential_chain_explicit_profile_provider() {
        let profile_provider = ProfileProvider::with_configuration(
            "tests/sample-data/multiple_profile_credentials",
            "foo",
        );

        let chain = ChainProvider::with_profile_provider(profile_provider);

        let credentials = chain.credentials().expect(
            "Failed to get credentials from default provider chain with manual profile",
        );

        assert_eq!(credentials.aws_access_key_id(), "foo_access_key");
        assert_eq!(credentials.aws_secret_access_key(), "foo_secret_key");
    }

    #[test]
    fn parse_iam_task_credentials_sample_response() {
        fn read_file_to_string(file_path: &Path) -> String {
            match fs::metadata(file_path) {
                Ok(metadata) => {
                    if !metadata.is_file() {
                        panic!("Couldn't open file");
                    }
                }
                Err(_) => panic!("Couldn't stat file"),
            };

            let mut file = File::open(file_path).unwrap();
            let mut result = String::new();
            file.read_to_string(&mut result).ok();

            result
        }

        let response = read_file_to_string(Path::new("tests/sample-data/iam_task_credentials_sample_response"));

        let credentials = parse_credentials_from_aws_service(&response);

        assert!(credentials.is_ok());
        let credentials = credentials.unwrap();

        assert_eq!(credentials.aws_access_key_id(), "AKIAIOSFODNN7EXAMPLE");
        assert_eq!(credentials.aws_secret_access_key(), "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY");
    }
}
