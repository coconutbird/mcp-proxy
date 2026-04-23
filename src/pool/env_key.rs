//! Env-key encoding for backend pool keys.
//!
//! Each backend entry in the pool is keyed by `(server_name, scope)` where
//! `scope` is derived from the server's sharing mode:
//!
//! - [`Sharing::Global`] → [`DEFAULT_ENV_KEY`] (one instance process-wide).
//! - [`Sharing::Session`] → `"session:<session_id>"` (one per HTTP session).
//! - [`Sharing::Credentials`] → `"env:<hash>"` of the server's relevant env
//!   overrides (one per distinct credential set).

use std::collections::HashMap;

use crate::config::{ServerConfig, Sharing, extract_var_names};
use crate::util::fnv1a;

/// Scope for backends with no per-client env overrides (Global sharing, or
/// Credentials sharing when the client supplied none of the relevant vars).
pub(crate) const DEFAULT_ENV_KEY: &str = "__default__";

/// Scope prefix for per-session backends (`session:<sid>`).
pub(crate) const SESSION_KEY_PREFIX: &str = "session:";

/// Scope prefix for credential-scoped backends (`env:<hash>`).
pub(crate) const ENV_KEY_PREFIX: &str = "env:";

/// Extract the `${VAR}` references from a server config's env values and args.
pub fn relevant_env_keys(srv: &ServerConfig) -> Vec<String> {
    let mut keys = Vec::new();
    for val in srv.env.values() {
        keys.extend(extract_var_names(val));
    }
    for arg in &srv.args {
        keys.extend(extract_var_names(arg));
    }
    keys.sort();
    keys.dedup();
    keys
}

/// Build the env-hash portion of a backend key, using only the env vars that
/// this server actually references.
pub(crate) fn backend_env_key(
    srv: &ServerConfig,
    env_overrides: &HashMap<String, String>,
) -> String {
    let keys = relevant_env_keys(srv);
    let relevant: Vec<(&str, &str)> = keys
        .iter()
        .filter_map(|k| {
            env_overrides
                .get(k.as_str())
                .map(|v| (k.as_str(), v.as_str()))
        })
        .collect();
    if relevant.is_empty() {
        return DEFAULT_ENV_KEY.to_string();
    }
    let s: String = relevant
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\0");
    format!("{ENV_KEY_PREFIX}{:x}", fnv1a(&s))
}

/// Compute the full scope key for `(server, env_overrides, session_id)` based
/// on the server's sharing mode.
pub(crate) fn sharing_env_key(
    srv: &ServerConfig,
    env_overrides: &HashMap<String, String>,
    session_id: &str,
) -> String {
    match srv.shared {
        Sharing::Global => DEFAULT_ENV_KEY.to_string(),
        Sharing::Credentials => backend_env_key(srv, env_overrides),
        Sharing::Session => format!("{SESSION_KEY_PREFIX}{session_id}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_srv(env: Vec<(&str, &str)>, args: Vec<&str>, sharing: Sharing) -> ServerConfig {
        ServerConfig {
            command: "echo".into(),
            args: args.into_iter().map(String::from).collect(),
            env: env
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            shared: sharing,
            ..Default::default()
        }
    }

    #[test]
    fn relevant_env_keys_deduplicates() {
        let srv = test_srv(
            vec![("X", "${TOK}"), ("Y", "${TOK}")],
            vec![],
            Sharing::Session,
        );
        assert_eq!(relevant_env_keys(&srv), vec!["TOK"]);
    }

    #[test]
    fn relevant_env_keys_from_args_and_env() {
        let srv = test_srv(vec![("X", "${A}")], vec!["--flag=${B}"], Sharing::Session);
        assert_eq!(relevant_env_keys(&srv), vec!["A", "B"]);
    }

    #[test]
    fn backend_env_key_default_when_no_overrides() {
        let srv = test_srv(vec![("X", "${TOK}")], vec![], Sharing::Credentials);
        assert_eq!(backend_env_key(&srv, &HashMap::new()), DEFAULT_ENV_KEY);
    }

    #[test]
    fn backend_env_key_hashes_when_overrides_present() {
        let srv = test_srv(vec![("X", "${TOK}")], vec![], Sharing::Credentials);
        let env = HashMap::from([("TOK".to_string(), "secret123".to_string())]);
        let key = backend_env_key(&srv, &env);
        assert!(
            key.starts_with(ENV_KEY_PREFIX),
            "expected env: prefix, got {key}"
        );
    }

    #[test]
    fn backend_env_key_same_creds_same_hash() {
        let srv = test_srv(vec![("X", "${TOK}")], vec![], Sharing::Credentials);
        let env1 = HashMap::from([("TOK".to_string(), "abc".to_string())]);
        let env2 = HashMap::from([("TOK".to_string(), "abc".to_string())]);
        assert_eq!(backend_env_key(&srv, &env1), backend_env_key(&srv, &env2));
    }

    #[test]
    fn backend_env_key_different_creds_different_hash() {
        let srv = test_srv(vec![("X", "${TOK}")], vec![], Sharing::Credentials);
        let env1 = HashMap::from([("TOK".to_string(), "abc".to_string())]);
        let env2 = HashMap::from([("TOK".to_string(), "xyz".to_string())]);
        assert_ne!(backend_env_key(&srv, &env1), backend_env_key(&srv, &env2));
    }

    #[test]
    fn backend_env_key_ignores_irrelevant_overrides() {
        let srv = test_srv(vec![("X", "${TOK}")], vec![], Sharing::Credentials);
        let env = HashMap::from([
            ("TOK".to_string(), "val".to_string()),
            ("UNRELATED".to_string(), "noise".to_string()),
        ]);
        let env2 = HashMap::from([("TOK".to_string(), "val".to_string())]);
        assert_eq!(backend_env_key(&srv, &env), backend_env_key(&srv, &env2));
    }
}
