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
