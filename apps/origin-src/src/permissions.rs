//! Who calls the service: the node's own user, an anonymous client, or a
//! principal the server [authenticated](crate::auth), whose token may
//! narrow their rights to its scope.

use origin_shared::Principal;
use origin_shared::access::Capability;

/// Who calls the service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Caller {
    /// The node's own user, through the command line or a remote on a
    /// local path: they may do everything.
    Owner,
    /// A client of a server that didn't authenticate.
    Anonymous,
    /// An authenticated client of a server.
    Principal {
        /// Who they are.
        principal: Principal,
        /// What their token narrows their rights to, if it's a scoped one.
        scope: Option<Scope>,
    },
}

/// What a scoped token narrows its bearer's rights to, and until when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    /// The capabilities it keeps of its bearer's rights.
    pub capabilities: Vec<Capability>,
    /// When it expires, in seconds since the Unix epoch: a token issued
    /// with it expires no later.
    pub expires: u64,
}

impl Caller {
    /// Who the caller is, unless the owner or anonymous.
    #[must_use]
    pub fn principal(&self) -> Option<&Principal> {
        match self {
            Self::Principal { principal, .. } => Some(principal),
            Self::Owner | Self::Anonymous => None,
        }
    }
}
