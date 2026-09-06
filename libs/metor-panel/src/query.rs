//! The one search language every filter bar in the panel speaks.
//!
//! A query is the text an operator typed into a bar, read as free-text
//! terms and `field:value` atoms. A term with a wildcard is the browser's
//! glob (`*.health`, anchored, spanning segments); a bare term is a
//! substring, so `imu` finds `cube_sat.imu.temp` without the operator
//! spelling out `*imu*`. Every term must match. Atoms are for the panels
//! whose rows have structure beyond a name — a log's `source:`, a
//! sequence's `state:` — and a panel that has no such field ignores them.
//!
//! Matching is case-insensitive throughout: component names are lowercase
//! by convention and nobody wants to remember which ones aren't.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use regex::Regex;

/// One free-text term, already compiled.
#[derive(Clone, Debug)]
enum Term {
    Glob(Regex),
    Substring(String),
}

impl Term {
    fn matches(&self, name: &str) -> bool {
        match self {
            Term::Glob(regex) => regex.is_match(name),
            Term::Substring(needle) => name.to_lowercase().contains(needle),
        }
    }
}

/// A parsed filter-bar query. Cheap to clone; compile once per edit and
/// match per row.
#[derive(Clone, Debug, Default)]
pub struct Query {
    text: String,
    terms: Vec<Term>,
    atoms: Vec<(String, String)>,
}

impl Query {
    /// Read `text` as whitespace-separated terms and `field:value` atoms.
    /// Never fails: a glob is escaped except for its wildcards, so there is
    /// no way to write an invalid one.
    pub fn parse(text: &str) -> Self {
        let mut terms = Vec::new();
        let mut atoms = Vec::new();
        for word in text.split_whitespace() {
            match word.split_once(':') {
                Some((field, value)) if !field.is_empty() && !value.is_empty() => {
                    atoms.push((field.to_lowercase(), value.to_lowercase()));
                }
                _ => terms.push(if word.contains(['*', '?']) {
                    Term::Glob(glob_to_regex(word))
                } else {
                    Term::Substring(word.to_lowercase())
                }),
            }
        }
        Self {
            text: text.to_string(),
            terms,
            atoms,
        }
    }

    /// The text as typed, for display and persistence.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// `true` when the query would match everything.
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty() && self.atoms.is_empty()
    }

    /// Whether any free-text term was typed, as opposed to atoms alone.
    pub fn has_terms(&self) -> bool {
        !self.terms.is_empty()
    }

    /// Whether `name` satisfies every free-text term. Atoms are not
    /// consulted — a panel checks those against its own fields.
    pub fn matches_name(&self, name: &str) -> bool {
        self.terms.iter().all(|term| term.matches(name))
    }

    /// The value of the first `field:` atom, lowercased.
    pub fn atom(&self, field: &str) -> Option<&str> {
        self.atoms
            .iter()
            .find(|(f, _)| f == field)
            .map(|(_, v)| v.as_str())
    }
}

/// Translate a glob (`*`, `?`) into an anchored, case-insensitive regex.
///
/// Everything that isn't a wildcard passes through `regex::escape`, so dots
/// in component names match literally. `*` maps to `.*` and `?` to `.` —
/// both cross segment boundaries, which is what an operator typing
/// `*.health` expects of `cube_sat.imu.health`.
pub fn glob_to_regex(pattern: &str) -> Regex {
    let mut out = String::with_capacity(pattern.len() + 6);
    out.push_str("(?i)^");
    for ch in pattern.chars() {
        match ch {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            other => out.push_str(&regex::escape(other.encode_utf8(&mut [0; 4]))),
        }
    }
    out.push('$');
    Regex::new(&out).expect("escaped glob is always a valid regex")
}

/// Fuzzy-score each haystack against `query` the way the inspector's list
/// does (`Pattern::parse`, so a spaced query narrows by every word). `None`
/// is a miss; higher is better.
pub fn fuzzy_scores<S: AsRef<str>>(
    query: &str,
    haystacks: impl IntoIterator<Item = S>,
) -> Vec<Option<u32>> {
    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut buf = Vec::new();
    haystacks
        .into_iter()
        .map(|haystack| {
            let haystack = Utf32Str::new(haystack.as_ref(), &mut buf);
            pattern.score(haystack, &mut matcher)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_term_is_a_substring() {
        let q = Query::parse("imu");
        assert!(q.matches_name("cube_sat.imu.temp"));
        assert!(q.matches_name("IMU"));
        assert!(!q.matches_name("cube_sat.gyro"));
    }

    #[test]
    fn a_wildcard_term_is_an_anchored_glob() {
        let q = Query::parse("*.health");
        assert!(q.matches_name("cube_sat.imu.health"));
        assert!(!q.matches_name("cube_sat.imu.health.flag"));
        assert!(Query::parse("cube_sat.?mu").matches_name("cube_sat.imu"));
        assert!(!Query::parse("imu").matches_name("gyro"));
    }

    #[test]
    fn every_term_must_match() {
        let q = Query::parse("imu temp");
        assert!(q.matches_name("cube_sat.imu.temp"));
        assert!(!q.matches_name("cube_sat.imu.accel"));
    }

    #[test]
    fn atoms_are_split_off_and_lowercased() {
        let q = Query::parse("source:IMU boot level:warn");
        assert_eq!(q.atom("source"), Some("imu"));
        assert_eq!(q.atom("level"), Some("warn"));
        assert_eq!(q.atom("state"), None);
        assert!(q.has_terms());
        assert!(q.matches_name("cold boot"));
        assert!(!Query::parse("source:imu").has_terms());
    }

    #[test]
    fn a_lone_colon_is_text() {
        let q = Query::parse(":");
        assert!(q.has_terms());
        assert!(q.matches_name("a:b"));
        assert!(Query::parse("").is_empty());
        assert!(Query::parse("   ").is_empty());
    }

    #[test]
    fn regex_metacharacters_in_a_glob_are_literal() {
        assert!(Query::parse("a.b*").matches_name("a.b.c"));
        assert!(!Query::parse("a.b*").matches_name("axb.c"));
        assert!(Query::parse("v(1)*").matches_name("v(1).x"));
    }

    #[test]
    fn fuzzy_scores_miss_and_rank() {
        let scores = fuzzy_scores("imu", ["cube_sat.imu", "gyro", "i_m_u"]);
        assert!(scores[0].is_some());
        assert!(scores[1].is_none());
        assert!(scores[0] > scores[2]);
    }
}
