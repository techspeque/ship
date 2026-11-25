//! Compose project naming (ADR-0001 rule 4).

use crate::id::EnvironmentId;

/// Prefix of every Compose project name Ship creates.
pub const COMPOSE_PROJECT_PREFIX: &str = "ship_";

/// The Compose project name for an environment: `ship_<environment id>`.
///
/// Its only input is the immutable environment id, so it is deterministic,
/// never contains user input (slugs, file names, manifest values), and always
/// matches Compose's `^[a-z0-9][a-z0-9_-]*$`.
pub fn compose_project_name(environment: &EnvironmentId) -> String {
    format!("{COMPOSE_PROJECT_PREFIX}{environment}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_valid_compose_name(s: &str) -> bool {
        let b = s.as_bytes();
        !b.is_empty()
            && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
            && b.iter()
                .all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
    }

    #[test]
    fn deterministic_and_valid() {
        let env = EnvironmentId::from_parts(1_469_918_176_385, 0xabcdef).unwrap();
        let name = compose_project_name(&env);
        assert_eq!(name, compose_project_name(&env));
        assert_eq!(name, format!("ship_{env}"));
        assert!(is_valid_compose_name(&name), "{name}");

        let parsed: EnvironmentId = env.to_string().parse().unwrap();
        assert_eq!(compose_project_name(&parsed), name);
    }

    #[test]
    fn valid_at_extremes() {
        for (ts, e) in [(0, 0), (crate::id::MAX_TIMESTAMP_MS, u128::MAX)] {
            let env = EnvironmentId::from_parts(ts, e).unwrap();
            assert!(is_valid_compose_name(&compose_project_name(&env)));
        }
    }

    #[test]
    fn distinct_ids_give_distinct_names() {
        let a = EnvironmentId::from_parts(1, 1).unwrap();
        let b = EnvironmentId::from_parts(1, 2).unwrap();
        assert_ne!(compose_project_name(&a), compose_project_name(&b));
    }
}
