//! Scope carried in the query string of every page and API call.

use crate::store::ScopeArgs;
use serde::Deserialize;

/// `?project=&all=&session=&since=&until=&captured_only=` as the browser
/// sends them. Every field is optional; booleans accept `1`, `true`, `on`,
/// `yes`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct ScopeQuery {
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub all: Option<String>,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub until: Option<String>,
    #[serde(default)]
    pub captured_only: Option<String>,
}

pub fn flag(v: &Option<String>) -> bool {
    matches!(
        v.as_deref().map(str::trim),
        Some("1") | Some("true") | Some("on") | Some("yes")
    )
}

fn non_empty(v: &Option<String>) -> Option<String> {
    v.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

impl ScopeQuery {
    /// Build from the parsed query pairs (duplicate keys keep the last one).
    pub fn from_map(q: &std::collections::HashMap<String, String>) -> Self {
        Self {
            project: q.get("project").cloned(),
            all: q.get("all").cloned(),
            session: q.get("session").cloned(),
            since: q.get("since").cloned(),
            until: q.get("until").cloned(),
            captured_only: q.get("captured_only").cloned(),
        }
    }

    pub fn args(&self) -> ScopeArgs {
        ScopeArgs {
            project: non_empty(&self.project),
            all_projects: flag(&self.all),
            session: non_empty(&self.session),
            since: non_empty(&self.since),
            until: non_empty(&self.until),
            captured_only: flag(&self.captured_only),
        }
    }

    pub fn all_projects(&self) -> bool {
        flag(&self.all)
    }

    pub fn captured_only(&self) -> bool {
        flag(&self.captured_only)
    }

    /// The scope as `key=value` pairs (only the set ones).
    pub fn pairs(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        if let Some(p) = non_empty(&self.project) {
            out.push(("project", p));
        }
        if self.all_projects() {
            out.push(("all", "1".to_string()));
        }
        if let Some(s) = non_empty(&self.session) {
            out.push(("session", s));
        }
        if let Some(s) = non_empty(&self.since) {
            out.push(("since", s));
        }
        if let Some(s) = non_empty(&self.until) {
            out.push(("until", s));
        }
        if self.captured_only() {
            out.push(("captured_only", "1".to_string()));
        }
        out
    }

    /// Query string (`?a=b&c=d`, or empty) for the scope plus `extra`.
    pub fn query_string(&self, extra: &[(&str, &str)]) -> String {
        let mut parts: Vec<String> = self
            .pairs()
            .into_iter()
            .map(|(k, v)| format!("{k}={}", crate::html::urlenc(&v)))
            .collect();
        for (k, v) in extra {
            parts.push(format!(
                "{}={}",
                crate::html::urlenc(k),
                crate::html::urlenc(v)
            ));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("?{}", parts.join("&"))
        }
    }

    /// Same scope without the session restriction (page links that must not
    /// inherit a single-session scope).
    pub fn without_session(&self) -> ScopeQuery {
        ScopeQuery {
            session: None,
            ..self.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_string_round_trip() {
        let s = ScopeQuery {
            project: Some("acme/repo".into()),
            captured_only: Some("1".into()),
            ..Default::default()
        };
        assert_eq!(
            s.query_string(&[("page", "2")]),
            "?project=acme%2Frepo&captured_only=1&page=2"
        );
        assert_eq!(ScopeQuery::default().query_string(&[]), "");
        assert!(s.args().captured_only);
        assert!(!s.args().all_projects);
    }
}
