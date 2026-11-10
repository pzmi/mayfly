//! What the auth chain needs to know about the App and where it is installed (spec §1).

use std::fmt;

use mayfly_domain::SecretRef;
use url::Url;

/// Where runners register: the registration-token path and the config URL (spec §1 steps 3
/// and 4). Enterprise scope comes later (ADR 0040).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// An organisation, the production scope.
    Organization {
        /// The organisation's login.
        org: String,
    },
    /// One repository, for tests.
    Repository {
        /// The owner's login.
        owner: String,
        /// The repository's name.
        repo: String,
    },
}

impl Scope {
    fn names(&self) -> Vec<&str> {
        match self {
            Self::Organization { org } => vec![org],
            Self::Repository { owner, repo } => vec![owner, repo],
        }
    }
}

/// The GitHub App and its installation. Holds a reference to the key, never the key (ADR 0033).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppConfig {
    /// The REST base: `https://api.github.com`, or `https://<host>/api/v3` for GHES (spec §1).
    pub api_base: Url,
    /// The web base the config URL is built on: `https://github.com`, or the GHES host.
    pub web_base: Url,
    /// The App's client id, the JWT's issuer (the numeric app id is also accepted [docs]).
    pub client_id: String,
    /// The installation of the App in the scope's owner.
    pub installation_id: u64,
    /// Where runners register.
    pub scope: Scope,
    /// Where the App's private key (PEM) comes from.
    pub app_key: SecretRef,
}

/// A configuration the chain refuses to run with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// A base URL that is not `https`, or plain `http` to a host other than loopback (a token
    /// would cross the network in clear).
    InsecureBase {
        /// Which base: `api_base` or `web_base`.
        field: &'static str,
    },
    /// A base URL with a query, a fragment or user info.
    BaseShape {
        /// Which base.
        field: &'static str,
    },
    /// An empty client id.
    ClientId,
    /// Installation id 0.
    InstallationId,
    /// An organisation, owner or repository name outside `[A-Za-z0-9._-]{1,100}`.
    Name,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InsecureBase { field } => write!(
                f,
                "github.{field} must be an https URL (plain http only to a loopback host)"
            ),
            Self::BaseShape { field } => write!(
                f,
                "github.{field} must not carry a query, a fragment or user info"
            ),
            Self::ClientId => f.write_str("github.client_id is empty"),
            Self::InstallationId => f.write_str("github.installation_id must be positive"),
            Self::Name => f.write_str(
                "an organisation, owner or repository name may hold only letters, digits, '.', '_' and '-'",
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

impl AppConfig {
    /// Checks the configuration before any request is made. Pinned by
    /// `config::tests::should_refuse_when_a_base_is_plain_http_to_a_remote_host`.
    ///
    /// # Errors
    /// The first [`ConfigError`] found.
    pub fn validate(&self) -> Result<(), ConfigError> {
        for (field, base) in [("api_base", &self.api_base), ("web_base", &self.web_base)] {
            check_base(field, base)?;
        }
        if self.client_id.trim().is_empty() {
            return Err(ConfigError::ClientId);
        }
        if self.installation_id == 0 {
            return Err(ConfigError::InstallationId);
        }
        if !self.scope.names().into_iter().all(valid_name) {
            return Err(ConfigError::Name);
        }
        Ok(())
    }

    /// `POST {api}/app/installations/{id}/access_tokens` (spec §1 step 2).
    #[must_use]
    pub fn installation_token_url(&self) -> Url {
        let id = self.installation_id.to_string();
        join(
            &self.api_base,
            &["app", "installations", &id, "access_tokens"],
        )
    }

    /// `POST {api}/orgs/{org}/actions/runners/registration-token`, or the repository's (spec §1
    /// step 3).
    #[must_use]
    pub fn registration_token_url(&self) -> Url {
        let tail = ["actions", "runners", "registration-token"];
        match &self.scope {
            Scope::Organization { org } => {
                join(&self.api_base, &[&["orgs", org], &tail[..]].concat())
            }
            Scope::Repository { owner, repo } => join(
                &self.api_base,
                &[&["repos", owner, repo], &tail[..]].concat(),
            ),
        }
    }

    /// `POST {api}/actions/runner-registration` (spec §1 step 4).
    #[must_use]
    pub fn runner_registration_url(&self) -> Url {
        join(&self.api_base, &["actions", "runner-registration"])
    }

    /// The organisation's or repository's web URL, the exchange's `url` field (spec §1 step 4).
    #[must_use]
    pub fn config_url(&self) -> Url {
        join(&self.web_base, &self.scope.names())
    }
}

fn check_base(field: &'static str, base: &Url) -> Result<(), ConfigError> {
    match credential_base_problem(base) {
        None => Ok(()),
        Some(BaseProblem::Insecure) => Err(ConfigError::InsecureBase { field }),
        Some(BaseProblem::Shape) => Err(ConfigError::BaseShape { field }),
    }
}

/// Why a URL may not receive a credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseProblem {
    /// Not `https`, and not plain `http` to a loopback host.
    Insecure,
    /// A query, a fragment or user info.
    Shape,
}

/// The one rule for every URL a credential is sent to: the configured bases and the URLs the
/// service hands back (the Actions service URL here, the message-queue URL in GH-03). It must
/// be `https`, or plain `http` to a loopback host (for the fake), with no query, fragment or
/// user info. Loopback means `127.0.0.0/8`, `::1` or the name `localhost`; the name is trusted
/// to resolve to loopback, which rests on the host's resolver (`/etc/hosts`), not on mayfly.
/// Pinned by `config::tests::should_refuse_when_a_base_is_plain_http_to_a_remote_host` and
/// `tests/client/auth.rs::should_refuse_the_service_url_when_a_token_could_leak_through_it`.
#[must_use]
pub fn credential_base_problem(base: &Url) -> Option<BaseProblem> {
    let loopback = matches!(
        base.host(),
        Some(url::Host::Ipv4(ip)) if ip.is_loopback()
    ) || matches!(base.host(), Some(url::Host::Ipv6(ip)) if ip.is_loopback())
        || base.host_str() == Some("localhost");
    let secure = base.scheme() == "https" || (base.scheme() == "http" && loopback);
    if !secure {
        return Some(BaseProblem::Insecure);
    }
    if base.query().is_some()
        || base.fragment().is_some()
        || !base.username().is_empty()
        || base.password().is_some()
    {
        return Some(BaseProblem::Shape);
    }
    None
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// `base` with `segments` appended to its path, each percent-encoded as one segment.
fn join(base: &Url, segments: &[&str]) -> Url {
    let mut url = base.clone();
    if let Ok(mut path) = url.path_segments_mut() {
        path.pop_if_empty().extend(segments);
    }
    url
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn config(api: &str, scope: Scope) -> AppConfig {
        AppConfig {
            api_base: Url::parse(api).expect("url"),
            web_base: Url::parse("https://github.com").expect("url"),
            client_id: "Iv1.synthetic".to_owned(),
            installation_id: 42,
            scope,
            app_key: SecretRef::File {
                path: PathBuf::from("/synthetic/key.pem"),
            },
        }
    }

    fn org(name: &str) -> Scope {
        Scope::Organization {
            org: name.to_owned(),
        }
    }

    #[test]
    fn should_build_each_endpoint_when_the_base_has_a_path() {
        // given
        let ghes = config("https://ghes.example/api/v3", org("acme"));
        let repo = config(
            "https://api.github.com/",
            Scope::Repository {
                owner: "acme".to_owned(),
                repo: "widgets".to_owned(),
            },
        );

        // when
        let urls = [
            ghes.installation_token_url().to_string(),
            ghes.registration_token_url().to_string(),
            ghes.runner_registration_url().to_string(),
            repo.registration_token_url().to_string(),
            repo.config_url().to_string(),
            ghes.config_url().to_string(),
        ];

        // then
        assert_eq!(
            urls,
            [
                "https://ghes.example/api/v3/app/installations/42/access_tokens",
                "https://ghes.example/api/v3/orgs/acme/actions/runners/registration-token",
                "https://ghes.example/api/v3/actions/runner-registration",
                "https://api.github.com/repos/acme/widgets/actions/runners/registration-token",
                "https://github.com/acme/widgets",
                "https://github.com/acme",
            ]
        );
    }

    #[test]
    fn should_refuse_when_a_base_is_plain_http_to_a_remote_host() {
        // given
        let cases = [
            (config("https://api.github.com", org("acme")), Ok(())),
            (config("http://127.0.0.1:8080", org("acme")), Ok(())),
            (config("http://[::1]:8080", org("acme")), Ok(())),
            (config("http://localhost:8080", org("acme")), Ok(())),
            (
                config("http://api.github.com", org("acme")),
                Err(ConfigError::InsecureBase { field: "api_base" }),
            ),
            (
                config("https://user:pw@api.github.com", org("acme")),
                Err(ConfigError::BaseShape { field: "api_base" }),
            ),
            (
                config("https://api.github.com/?a=b", org("acme")),
                Err(ConfigError::BaseShape { field: "api_base" }),
            ),
            (
                config("https://api.github.com", org("a/b")),
                Err(ConfigError::Name),
            ),
            (
                config("https://api.github.com", org("..")),
                Err(ConfigError::Name),
            ),
            (
                config("https://api.github.com", org("")),
                Err(ConfigError::Name),
            ),
        ];
        let mut no_client = config("https://api.github.com", org("acme"));
        no_client.client_id = " ".to_owned();
        let mut no_installation = config("https://api.github.com", org("acme"));
        no_installation.installation_id = 0;

        // when
        let got: Vec<_> = cases.iter().map(|(c, _)| c.validate()).collect();

        // then
        let expected: Vec<_> = cases.iter().map(|(_, e)| e.clone()).collect();
        assert_eq!(got, expected);
        assert_eq!(no_client.validate(), Err(ConfigError::ClientId));
        assert_eq!(no_installation.validate(), Err(ConfigError::InstallationId));
        assert!(ConfigError::Name.to_string().contains("letters"));
    }
}
