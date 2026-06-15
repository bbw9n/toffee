use serde::{Deserialize, Serialize};

/// A scope is an ordered set of opaque strings such as `project:magi`,
/// `user:me`, `repo:github.com/foo/bar`, `global`.
///
/// Scopes are compared as exact strings. Inheritance and expansion are a
/// runtime concern that the core layer does not enforce.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Scope(pub Vec<String>);

impl Scope {
    pub fn new<I, S>(items: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Scope(items.into_iter().map(Into::into).collect())
    }

    pub fn as_slice(&self) -> &[String] {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<Vec<String>> for Scope {
    fn from(v: Vec<String>) -> Self {
        Scope(v)
    }
}

impl From<Scope> for Vec<String> {
    fn from(s: Scope) -> Self {
        s.0
    }
}

/// Common scope strings.
pub const SCOPE_GLOBAL: &str = "global";
pub const SCOPE_USER_ME: &str = "user:me";

/// Expand a requested scope set to include inherited scopes. v1 rule:
/// if any explicit scope is supplied, we additionally consider `user:me` and
/// `global` so that user-level preferences and global facts surface for a
/// project-scoped request. The retrieval-time salience penalty is a
/// downstream concern (see RFC §11, open question 1).
///
/// Pure: deterministic on `requested`. The returned vector preserves the
/// order of `requested` and appends inherited scopes only if they weren't
/// already present.
pub fn expand_inherited(requested: &[String]) -> Vec<String> {
    let mut out: Vec<String> = requested.to_vec();
    if out.is_empty() {
        // Empty input means "no preference". Don't add anything — let the
        // caller decide policy (the daemon currently rejects empty scope
        // sets at the API boundary).
        return out;
    }
    let has_global = out.iter().any(|s| s == SCOPE_GLOBAL);
    let has_user = out.iter().any(|s| s == SCOPE_USER_ME);
    if !has_user {
        out.push(SCOPE_USER_ME.to_string());
    }
    if !has_global {
        out.push(SCOPE_GLOBAL.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_inherited_adds_user_and_global() {
        let out = expand_inherited(&["project:foo".to_string()]);
        assert_eq!(out, vec!["project:foo", "user:me", "global"]);
    }

    #[test]
    fn expand_inherited_is_idempotent() {
        let once = expand_inherited(&["project:foo".to_string()]);
        let twice = expand_inherited(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn expand_inherited_preserves_explicit_global() {
        let out = expand_inherited(&["global".to_string(), "project:foo".to_string()]);
        assert_eq!(out, vec!["global", "project:foo", "user:me"]);
    }

    #[test]
    fn expand_inherited_on_empty_is_empty() {
        assert!(expand_inherited(&[]).is_empty());
    }
}
