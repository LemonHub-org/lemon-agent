//! Capability tokens gate every privileged sandbox operation.
//!
//! A token is a set of capabilities, each scoped to a resource with read,
//! write, or execute permissions and an optional expiry. The main loop holds
//! the full token; any restricted caller must present a token that grants the
//! requested resource and permission.

use std::time::{Duration, Instant};

use crate::error::{Error, Result};

/// The resource a capability applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Resource {
    /// File operations within the sandbox root.
    FileSystem,
    /// External process execution from the command whitelist.
    Process,
    /// Git operations in the sandbox root.
    Git,
    /// LLM queries.
    Llm,
}

impl Resource {
    pub fn as_str(self) -> &'static str {
        match self {
            Resource::FileSystem => "fs",
            Resource::Process => "process",
            Resource::Git => "git",
            Resource::Llm => "llm",
        }
    }
}

/// Read, write, and execute permission flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Permissions(u8);

impl Permissions {
    const READ: u8 = 1;
    const WRITE: u8 = 2;
    const EXECUTE: u8 = 4;

    /// A read-only permission set.
    pub const fn read() -> Permissions {
        Permissions(Permissions::READ)
    }

    /// A write-only permission set.
    pub const fn write() -> Permissions {
        Permissions(Permissions::WRITE)
    }

    /// An execute-only permission set.
    pub const fn execute() -> Permissions {
        Permissions(Permissions::EXECUTE)
    }

    /// A read-write permission set.
    pub const fn read_write() -> Permissions {
        Permissions(Permissions::READ | Permissions::WRITE)
    }

    /// A full permission set.
    pub const fn all() -> Permissions {
        Permissions(Permissions::READ | Permissions::WRITE | Permissions::EXECUTE)
    }

    /// Whether `self` grants everything in `required`.
    pub fn contains(self, required: Permissions) -> bool {
        self.0 & required.0 == required.0
    }
}

/// A single capability: access to `resource` with `permissions`.
#[derive(Debug, Clone)]
pub struct Capability {
    pub resource: Resource,
    pub permissions: Permissions,
    expires_at: Option<Instant>,
}

impl Capability {
    pub fn new(resource: Resource, permissions: Permissions) -> Capability {
        Capability {
            resource,
            permissions,
            expires_at: None,
        }
    }

    /// Limit the capability's validity window.
    pub fn with_expiry(mut self, ttl: Duration) -> Capability {
        self.expires_at = Some(Instant::now() + ttl);
        self
    }

    /// Whether the capability is currently valid for the requested access.
    fn is_valid(&self, resource: Resource, permissions: Permissions) -> bool {
        self.resource == resource
            && self.permissions.contains(permissions)
            && self.expires_at.is_none_or(|expiry| Instant::now() < expiry)
    }
}

/// A token presented by a caller: a set of capabilities.
///
/// The default token (`full()`) grants everything and never expires. Restricted
/// tokens are derived with `with_only` or `with_expiry`.
#[derive(Debug, Clone, Default)]
pub struct CapabilitySet {
    caps: Vec<Capability>,
}

impl CapabilitySet {
    /// The full, non-expiring token held by the scheduler's main loop.
    pub fn full() -> CapabilitySet {
        CapabilitySet {
            caps: vec![
                Capability::new(Resource::FileSystem, Permissions::all()),
                Capability::new(Resource::Process, Permissions::all()),
                Capability::new(Resource::Git, Permissions::all()),
                Capability::new(Resource::Llm, Permissions::all()),
            ],
        }
    }

    /// A token restricted to the given capabilities.
    pub fn from_capabilities(caps: Vec<Capability>) -> CapabilitySet {
        CapabilitySet { caps }
    }

    /// Derive a token that only retains grants for `resource`.
    pub fn with_only(&self, resource: Resource) -> CapabilitySet {
        CapabilitySet {
            caps: self
                .caps
                .iter()
                .filter(|c| c.resource == resource)
                .cloned()
                .collect(),
        }
    }

    /// Derive a token that expires after `ttl`.
    pub fn with_expiry(&self, ttl: Duration) -> CapabilitySet {
        CapabilitySet {
            caps: self
                .caps
                .iter()
                .map(|c| Capability {
                    expires_at: Some(Instant::now() + ttl),
                    ..c.clone()
                })
                .collect(),
        }
    }

    /// Validate that this token grants `permissions` on `resource`.
    ///
    /// Fails with `CapabilityDenied` when the grant is missing or expired.
    pub fn require(&self, resource: Resource, permissions: Permissions) -> Result<()> {
        for cap in &self.caps {
            if cap.is_valid(resource, permissions) {
                return Ok(());
            }
        }
        Err(Error::CapabilityDenied {
            operation: format!("{}:{}", resource.as_str(), perm_str(permissions)),
            reason: "no valid capability covers this access".to_string(),
        })
    }
}

fn perm_str(p: Permissions) -> &'static str {
    if p.contains(Permissions::all()) {
        "read+write+execute"
    } else if p.contains(Permissions::read_write()) {
        "read+write"
    } else if p == Permissions::read() {
        "read"
    } else if p == Permissions::execute() {
        "execute"
    } else {
        "mixed"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_token_grants_everything() {
        let token = CapabilitySet::full();
        assert!(
            token
                .require(Resource::FileSystem, Permissions::all())
                .is_ok()
        );
        assert!(
            token
                .require(Resource::Process, Permissions::execute())
                .is_ok()
        );
        assert!(token.require(Resource::Llm, Permissions::all()).is_ok());
    }

    #[test]
    fn restricted_token_denies_other_resources() {
        let token = CapabilitySet::full().with_only(Resource::FileSystem);
        assert!(
            token
                .require(Resource::FileSystem, Permissions::read())
                .is_ok()
        );
        let err = token
            .require(Resource::Process, Permissions::execute())
            .unwrap_err();
        assert_eq!(
            err.code(),
            Error::CapabilityDenied {
                operation: String::new(),
                reason: String::new()
            }
            .code()
        );
    }

    #[test]
    fn read_only_token_denies_writes() {
        let token = CapabilitySet::from_capabilities(vec![Capability::new(
            Resource::FileSystem,
            Permissions::read(),
        )]);
        assert!(
            token
                .require(Resource::FileSystem, Permissions::read())
                .is_ok()
        );
        let err = token
            .require(Resource::FileSystem, Permissions::write())
            .unwrap_err();
        assert!(err.to_string().contains("E006"));
    }

    #[test]
    fn expired_token_is_denied() {
        let token = CapabilitySet::from_capabilities(vec![
            Capability::new(Resource::FileSystem, Permissions::all())
                .with_expiry(Duration::from_millis(1)),
        ]);
        std::thread::sleep(Duration::from_millis(5));
        let err = token
            .require(Resource::FileSystem, Permissions::read())
            .unwrap_err();
        assert!(err.to_string().contains("E006"));
    }
}
