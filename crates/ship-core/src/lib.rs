//! Ship domain types, state machines, config precedence and trust profile.
//!
//! The root of the workspace dependency graph: depends on no other workspace
//! crate and performs no I/O (OVERVIEW §2.3). Nothing here reads a clock, a
//! random source, the environment, a file or a socket; callers supply those
//! values.

use std::fmt;

/// A string did not name any variant of a fixed string enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownValue {
    /// The type that was being parsed.
    pub kind: &'static str,
}

impl fmt::Display for UnknownValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown {} value", self.kind)
    }
}

impl std::error::Error for UnknownValue {}

/// Defines a fieldless enum with one stable string per variant, used for
/// `Display`, `FromStr`, serde and storage alike.
macro_rules! string_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident => $s:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        $vis enum $name {
            $( $(#[$vmeta])* #[serde(rename = $s)] $variant ),+
        }

        impl $name {
            /// Every variant, in declaration order.
            pub const ALL: &'static [$name] = &[ $( $name::$variant ),+ ];

            /// The stable string form.
            pub fn as_str(self) -> &'static str {
                match self {
                    $( $name::$variant => $s ),+
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for $name {
            type Err = $crate::UnknownValue;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $( $s => Ok($name::$variant), )+
                    _ => Err($crate::UnknownValue { kind: stringify!($name) }),
                }
            }
        }
    };
}

pub mod api;
pub mod compose;
pub mod error;
pub mod id;
pub mod model;
pub mod secret;
pub mod slug;
pub mod state;
pub mod time;

pub use api::{
    API_PREFIX, API_VERSION, API_VERSION_HEADER, DockerInfo, DockerStatus, HostCapacity, HostInfo,
    ProjectList, ProjectSummary, SHIP_VERSION_HEADER, SupportedVersions, api_version_number,
};
pub use compose::compose_project_name;
pub use error::{ApiError, ErrorCode};
pub use id::{
    DeploymentId, EnvironmentId, IdError, OperationId, ProjectId, ReleaseId, RevisionId, ServiceId,
};
pub use model::{RecoveryPolicy, ServiceKind};
pub use secret::SecretValue;
pub use slug::{Slug, SlugError};
pub use state::{
    ContainerState, DesiredState, HealthCheck, Observation, OperationPhase, OperationStatus,
    ServiceStatus, TransitionError,
};
pub use time::{Timestamp, TimestampError};
