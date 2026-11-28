//! Default exclusions (ADR-0005 rule 2): files and directories that never
//! leave the operator's machine unless named explicitly.
//!
//! Matching is by entry name at any depth, ASCII case-insensitive, so a
//! case-insensitive filesystem cannot slip `.ENV` past the rule.

use serde::Serialize;

/// Why an entry is excluded by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Exclusion {
    /// `.git`, a directory or a worktree's `.git` file.
    GitMetadata,
    /// `.env` and `.env.*`, except template files such as `.env.example`.
    EnvFile,
    /// SSH private keys and key or keystore files.
    PrivateKey,
    /// Files and directories that hold credentials for other tools.
    CredentialStore,
    /// Dependency and build output directories.
    BuildDirectory,
}

impl Exclusion {
    /// The stable string of this kind, as serialised.
    pub fn as_str(self) -> &'static str {
        match self {
            Exclusion::GitMetadata => "git_metadata",
            Exclusion::EnvFile => "env_file",
            Exclusion::PrivateKey => "private_key",
            Exclusion::CredentialStore => "credential_store",
            Exclusion::BuildDirectory => "build_directory",
        }
    }
}

/// `.env.*` names that are templates and are sent.
const ENV_TEMPLATES: &[&str] = &[".env.example", ".env.sample", ".env.template", ".env.dist"];

/// SSH private key file names (their `.pub` halves are sent).
const SSH_KEYS: &[&str] = &["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"];

/// Extensions of key and keystore files.
const KEY_EXTENSIONS: &[&str] = &[".pem", ".key", ".p12", ".pfx", ".jks", ".keystore"];

/// Credential files.
const CREDENTIAL_FILES: &[&str] = &[
    ".netrc",
    "_netrc",
    ".pgpass",
    ".git-credentials",
    ".npmrc",
    ".pypirc",
];

/// Credential directories.
const CREDENTIAL_DIRS: &[&str] = &[".ssh", ".aws", ".gnupg", ".kube", ".docker"];

/// Build and dependency directories.
const BUILD_DIRS: &[&str] = &["node_modules", "target"];

/// The default exclusion that applies to an entry called `name`, if any.
pub fn classify(name: &str, is_dir: bool) -> Option<Exclusion> {
    let lower = name.to_ascii_lowercase();
    let name = lower.as_str();

    if name == ".git" {
        return Some(Exclusion::GitMetadata);
    }
    if name == ".env" || (name.starts_with(".env.") && !ENV_TEMPLATES.contains(&name)) {
        return Some(Exclusion::EnvFile);
    }
    if is_dir {
        if CREDENTIAL_DIRS.contains(&name) {
            return Some(Exclusion::CredentialStore);
        }
        if BUILD_DIRS.contains(&name) {
            return Some(Exclusion::BuildDirectory);
        }
        return None;
    }
    if SSH_KEYS.contains(&name) || KEY_EXTENSIONS.iter().any(|ext| name.ends_with(ext)) {
        return Some(Exclusion::PrivateKey);
    }
    if CREDENTIAL_FILES.contains(&name) {
        return Some(Exclusion::CredentialStore);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_metadata_as_directory_or_file() {
        assert_eq!(classify(".git", true), Some(Exclusion::GitMetadata));
        assert_eq!(classify(".git", false), Some(Exclusion::GitMetadata));
        assert_eq!(classify(".gitignore", false), None);
        assert_eq!(classify(".github", true), None);
    }

    #[test]
    fn env_files_excluded_and_templates_kept() {
        for name in [
            ".env",
            ".env.local",
            ".env.production",
            ".ENV",
            ".Env.Local",
        ] {
            assert_eq!(classify(name, false), Some(Exclusion::EnvFile), "{name}");
        }
        assert_eq!(classify(".env", true), Some(Exclusion::EnvFile));
        for name in ENV_TEMPLATES {
            assert_eq!(classify(name, false), None, "{name}");
        }
        assert_eq!(classify(".envrc", false), None);
        assert_eq!(classify("env", false), None);
        assert_eq!(classify("prod.env", false), None);
    }

    #[test]
    fn private_keys_excluded_and_public_halves_kept() {
        for name in [
            "id_rsa",
            "id_ed25519",
            "ID_ECDSA",
            "server.pem",
            "tls.key",
            "cert.p12",
            "cert.pfx",
            "store.jks",
            "release.keystore",
        ] {
            assert_eq!(classify(name, false), Some(Exclusion::PrivateKey), "{name}");
        }
        assert_eq!(classify("id_rsa.pub", false), None);
        assert_eq!(classify("id_ed25519.pub", false), None);
        assert_eq!(classify("keys.rs", false), None);
        assert_eq!(classify("monkey", false), None);
    }

    #[test]
    fn credential_stores_excluded() {
        for name in CREDENTIAL_FILES {
            assert_eq!(
                classify(name, false),
                Some(Exclusion::CredentialStore),
                "{name}"
            );
        }
        for name in CREDENTIAL_DIRS {
            assert_eq!(
                classify(name, true),
                Some(Exclusion::CredentialStore),
                "{name}"
            );
            assert_eq!(classify(name, false), None, "{name} as a file");
        }
    }

    #[test]
    fn build_directories_excluded_only_as_directories() {
        assert_eq!(
            classify("node_modules", true),
            Some(Exclusion::BuildDirectory)
        );
        assert_eq!(classify("target", true), Some(Exclusion::BuildDirectory));
        assert_eq!(classify("target", false), None);
        assert_eq!(classify("targets", true), None);
        assert_eq!(classify("src", true), None);
    }

    #[test]
    fn kind_strings_are_stable() {
        let all = [
            Exclusion::GitMetadata,
            Exclusion::EnvFile,
            Exclusion::PrivateKey,
            Exclusion::CredentialStore,
            Exclusion::BuildDirectory,
        ];
        let strings: Vec<&str> = all.iter().map(|k| k.as_str()).collect();
        assert_eq!(
            strings,
            [
                "git_metadata",
                "env_file",
                "private_key",
                "credential_store",
                "build_directory"
            ]
        );
    }
}
