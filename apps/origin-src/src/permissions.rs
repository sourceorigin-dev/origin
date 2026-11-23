//! Identifies who calls the service: the node's own user, an anonymous
//! client, or a principal the server [authenticated](crate::auth). A
//! principal's token may narrow their rights to a scope.

use origin_shared::Principal;
use origin_shared::access::Capability;

/// Who calls the service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Caller {
    /// The node's own user, through the command line or a remote on a local
    /// path. The owner may do everything.
    Owner,
    /// A client of a server that sent no credentials.
    Anonymous,
    /// An authenticated client of a server.
    Principal {
        /// Who the client is.
        principal: Principal,
        /// The scope their token narrows their rights to, if the token is scoped.
        scope: Option<Scope>,
    },
}

/// The rights a scoped token narrows its bearer to, and when it expires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    /// The capabilities of its bearer's rights that the token keeps.
    pub capabilities: Vec<Capability>,
    /// When the token expires, in seconds since the Unix epoch. A token issued
    /// by its bearer expires no later.
    pub expires: u64,
}

impl Caller {
    /// The caller's principal, or `None` for the owner or an anonymous caller.
    #[must_use]
    pub fn principal(&self) -> Option<&Principal> {
        match self {
            Self::Principal { principal, .. } => Some(principal),
            Self::Owner | Self::Anonymous => None,
        }
    }
}

/// Something a caller may not do.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Forbidden {
    /// The owner has no principal to issue a token for.
    #[error("only a signed-in caller gets a token, for themselves")]
    NoPrincipal,
    /// A token's scope reaches beyond its issuer's rights.
    #[error("a token may only narrow your rights, which don't include `{0}`")]
    Scope(Capability),
}
