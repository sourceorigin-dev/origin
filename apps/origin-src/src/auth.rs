//! Authenticates callers of the HTTP API by their bearer token: either a
//! scoped, short-lived token the server signed (see [`Token`]), or a static one
//! from its configuration, which an [`Authenticator`] checks.

use std::collections::BTreeMap;
use std::fmt;

use origin_shared::access::{InvalidToken, Token};
use origin_shared::{NodePublicKey, Principal};
use serde::Deserialize;

use crate::permissions::Scope;
use crate::service::Caller;

/// Maps static credentials to principals. The server configuration's tokens are
/// one implementation; external identity providers can be others.
pub trait Authenticator: Send + Sync + fmt::Debug {
    /// Returns the principal a bearer token belongs to, or `None` if it belongs to
    /// no one.
    fn authenticate(&self, token: &str) -> Option<Principal>;
}

/// Static tokens, from the server configuration:
///
/// ```toml
/// [tokens]
/// "user:alice" = "a long random secret"
/// "node:ci-1"  = "another one"
/// ```
#[derive(Default, Deserialize)]
pub struct StaticTokens(BTreeMap<Principal, String>);

impl fmt::Debug for StaticTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        crate::config::debug_keys(&self.0, f)
    }
}

impl Authenticator for StaticTokens {
    fn authenticate(&self, token: &str) -> Option<Principal> {
        // INVARIANT: hashes compare in constant time, so response times don't reveal how much of a token matched.
        let token = blake3::hash(token.as_bytes());
        self.0
            .iter()
            .find(|(_, known)| blake3::hash(known.as_bytes()) == token)
            .map(|(principal, _)| principal.clone())
    }
}

/// How a server identifies its callers: its node's public key, which verifies
/// the tokens it signed, and its static credentials.
#[derive(Debug)]
pub struct Credentials {
    key: NodePublicKey,
    authenticator: Box<dyn Authenticator>,
}

impl Credentials {
    /// Creates the credentials for callers of the node whose public key is `key`;
    /// `authenticator` checks their static credentials.
    #[must_use]
    pub fn new(key: NodePublicKey, authenticator: Box<dyn Authenticator>) -> Self {
        Self { key, authenticator }
    }

    /// Returns the caller an `Authorization` header's value stands for, or an
    /// anonymous caller without one.
    ///
    /// # Errors
    ///
    /// Fails if the value isn't a bearer token, or the token isn't valid.
    pub fn caller(&self, authorization: Option<&str>) -> Result<Caller, InvalidToken> {
        let Some(authorization) = authorization else {
            return Ok(Caller::Anonymous);
        };
        let token = authorization
            .strip_prefix("Bearer ")
            .ok_or(InvalidToken::Malformed)?;
        if Token::is_signed(token) {
            let token = Token::verify(token, &self.key)?;
            return Ok(Caller::Principal {
                principal: token.principal,
                scope: Some(Scope {
                    capabilities: token.scope,
                    expires: token.expires,
                }),
            });
        }
        let principal = self
            .authenticator
            .authenticate(token)
            .ok_or(InvalidToken::Unknown)?;
        Ok(Caller::Principal {
            principal,
            scope: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use origin_shared::NodeKey;

    use super::*;

    #[test]
    fn tells_callers_apart_by_token() {
        let tokens: StaticTokens =
            toml::from_str("\"user:alice\" = \"a-secret\"\n\"node:ci\" = \"c-secret\"\n").unwrap();
        let key = NodeKey::from_bytes(&[5; 32]);
        let token = Token::new(
            "user:bob".parse().unwrap(),
            vec!["write src".parse().unwrap()],
            Duration::from_mins(1),
        );
        let signed = token.sign(&key);
        let credentials = Credentials::new(key.public(), Box::new(tokens));
        let caller = |header: Option<&str>| credentials.caller(header);
        let principal = |caller: Caller| caller.principal().map(ToString::to_string);

        assert_eq!(caller(None), Ok(Caller::Anonymous));
        assert_eq!(
            principal(caller(Some("Bearer a-secret")).unwrap()).as_deref(),
            Some("user:alice")
        );
        assert_eq!(
            principal(caller(Some("Bearer c-secret")).unwrap()).as_deref(),
            Some("node:ci")
        );
        let Ok(Caller::Principal { principal, scope }) = caller(Some(&format!("Bearer {signed}")))
        else {
            panic!("a signed token");
        };
        assert_eq!(principal.to_string(), "user:bob");
        let scope = scope.unwrap();
        assert_eq!(scope.capabilities[0].to_string(), "write src");
        assert_eq!(scope.expires, token.expires, "it carries its expiry");
        assert!(caller(Some("Bearer a-secre")).is_err());
        assert!(caller(Some("a-secret")).is_err(), "not a bearer token");
    }
}
