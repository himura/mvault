//! Clients for the sites models come from.
//!
//! A client is given the base URL of its site, so that tests can point it at
//! a server of their own. In production the base URL is `https://<host>`.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

pub mod civitai;
pub mod hf;

/// Everything but the characters that never need escaping in a URL.
const ESCAPED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Escapes `value` for use as one path segment of a URL.
pub(crate) fn encode_segment(value: &str) -> String {
    utf8_percent_encode(value, ESCAPED).to_string()
}

/// Escapes each segment of a `/`-separated path, keeping the separators.
fn encode_path(path: &str) -> String {
    path.split('/')
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// The base URL of a site and the credentials for it.
#[derive(Clone)]
struct Endpoint {
    base_url: String,
    authorization: Option<String>,
}

impl Endpoint {
    fn new(base_url: &str, token: Option<&str>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            authorization: token.map(|token| format!("Bearer {token}")),
        }
    }

    /// Whether `url` is on this site.
    fn contains(&self, url: &str) -> bool {
        url.strip_prefix(&self.base_url)
            .is_some_and(|rest| rest.starts_with('/'))
    }

    /// The headers for a request to `url`: the credentials if the URL is on
    /// this site, nothing otherwise.
    fn headers_for(&self, url: &str) -> Vec<(&str, &str)> {
        match &self.authorization {
            Some(value) if self.contains(url) => vec![("Authorization", value.as_str())],
            _ => Vec::new(),
        }
    }
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The token stays out of logs and panic messages.
        f.debug_struct("Endpoint")
            .field("base_url", &self.base_url)
            .field("authorization", &self.authorization.as_ref().map(|_| "…"))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_HEADERS: [(&str, &str); 0] = [];

    #[test]
    fn segments_and_paths_are_escaped() {
        assert_eq!(encode_segment("refs/pr/1"), "refs%2Fpr%2F1");
        assert_eq!(encode_segment("v1.0-rc_2~x"), "v1.0-rc_2~x");
        assert_eq!(
            encode_path("dir a/モデル#1.bin"),
            "dir%20a/%E3%83%A2%E3%83%87%E3%83%AB%231.bin"
        );
    }

    #[test]
    fn credentials_stay_on_the_site() {
        let endpoint = Endpoint::new("https://example.com/", Some("secret"));
        assert_eq!(
            endpoint.headers_for("https://example.com/api/x"),
            [("Authorization", "Bearer secret")]
        );
        assert_eq!(
            endpoint.headers_for("https://example.com.evil.example/api"),
            NO_HEADERS
        );
        assert_eq!(
            endpoint.headers_for("https://cdn.example.net/file"),
            NO_HEADERS
        );
        assert_eq!(endpoint.headers_for("http://example.com/api/x"), NO_HEADERS);

        let anonymous = Endpoint::new("https://example.com", None);
        assert_eq!(
            anonymous.headers_for("https://example.com/api/x"),
            NO_HEADERS
        );
    }

    #[test]
    fn debug_output_hides_the_token() {
        let endpoint = Endpoint::new("https://example.com", Some("secret"));
        assert!(!format!("{endpoint:?}").contains("secret"));
    }
}
