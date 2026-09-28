//! Prefixed identifiers. Clients must treat them as opaque strings.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Prefix every identifier of this kind starts with.
            pub const PREFIX: &'static str = $prefix;

            /// Generates a fresh identifier (prefix + ULID).
            pub fn generate() -> Self {
                Self(format!("{}{}", $prefix, ulid::Ulid::generate()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_string(self) -> String {
                self.0
            }

            /// Whether the string carries this identifier's prefix.
            pub fn has_valid_prefix(&self) -> bool {
                self.0.starts_with($prefix) && self.0.len() > $prefix.len()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

id_type!(
    /// Identifier of a project (`prj_…`).
    ProjectId,
    "prj_"
);
id_type!(
    /// Identifier of a thread (`thr_…`).
    ThreadId,
    "thr_"
);
id_type!(
    /// Identifier of a turn (`trn_…`).
    TurnId,
    "trn_"
);
id_type!(
    /// Identifier of an item (`itm_…`).
    ItemId,
    "itm_"
);
id_type!(
    /// Identifier of an interaction (approval or question, `int_…`).
    InteractionId,
    "int_"
);
id_type!(
    /// Identifier of a queued input (`que_…`).
    QueuedInputId,
    "que_"
);
id_type!(
    /// Identifier of a long-running server operation (`op_…`).
    OperationId,
    "op_"
);
id_type!(
    /// Identifier of a paired device (`dev_…`).
    DeviceId,
    "dev_"
);
id_type!(
    /// Identifier of a background task: work the harness runs outside the turn lifecycle
    /// (`bgt_…`).
    BackgroundTaskId,
    "bgt_"
);

/// Identifier of a blob: `blb_` followed by the lowercase hex SHA-256 of its content.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct BlobId(String);

impl BlobId {
    pub const PREFIX: &'static str = "blb_";

    /// Builds the identifier from a lowercase hex SHA-256 digest.
    pub fn from_sha256_hex(hex: &str) -> Self {
        Self(format!("{}{}", Self::PREFIX, hex))
    }

    /// Returns the hex digest if the identifier is well formed (64 lowercase hex chars).
    pub fn sha256_hex(&self) -> Option<&str> {
        let hex = self.0.strip_prefix(Self::PREFIX)?;
        (hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
        .then_some(hex)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BlobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for BlobId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for BlobId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_carry_prefix() {
        let id = ThreadId::generate();
        assert!(id.as_str().starts_with("thr_"));
        assert!(id.has_valid_prefix());
        assert_eq!(id.as_str().len(), 4 + 26);
        assert!(!ThreadId::from("prj_x").has_valid_prefix());
    }

    #[test]
    fn blob_id_validates_digest() {
        let hex = "a".repeat(64);
        let id = BlobId::from_sha256_hex(&hex);
        assert_eq!(id.sha256_hex(), Some(hex.as_str()));
        assert_eq!(BlobId::from("blb_xyz").sha256_hex(), None);
        assert_eq!(
            BlobId::from(format!("blb_{}", "A".repeat(64))).sha256_hex(),
            None
        );
    }
}
