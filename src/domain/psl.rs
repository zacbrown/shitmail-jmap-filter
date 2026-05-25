use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::Path;

/// In-memory Public Suffix List, sufficient for computing the
/// registrable domain (eTLD+1) of a hostname.
pub struct Psl {
    /// Normal rules: "co.uk", "com", "*.kawasaki.jp"
    rules: HashSet<String>,
    /// Exception rules: "city.kawasaki.jp" (rule "!city.kawasaki.jp")
    exceptions: HashSet<String>,
    /// All labels in `rules` that start with `*.` — used to match
    /// `something.<wildcard-parent>` as a public suffix.
    wildcard_parents: HashSet<String>,
}

impl Psl {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading PSL at {:?}", path))?;
        Ok(Self::parse(&text))
    }

    pub fn parse(text: &str) -> Self {
        let mut rules = HashSet::new();
        let mut exceptions = HashSet::new();
        let mut wildcard_parents = HashSet::new();

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with("//") {
                continue;
            }
            let rule = line.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
            if rule.is_empty() {
                continue;
            }
            if let Some(stripped) = rule.strip_prefix('!') {
                exceptions.insert(stripped.to_string());
            } else if let Some(stripped) = rule.strip_prefix("*.") {
                wildcard_parents.insert(stripped.to_string());
                rules.insert(rule);
            } else {
                rules.insert(rule);
            }
        }

        Self { rules, exceptions, wildcard_parents }
    }

    /// Returns the registrable domain (eTLD+1) of `host`, or `None` if
    /// `host` is itself a public suffix or has no registrable parent.
    pub fn registrable_domain(&self, host: &str) -> Option<String> {
        let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
        if host.is_empty() {
            return None;
        }
        let labels: Vec<&str> = host.split('.').collect();
        if labels.iter().any(|l| l.is_empty()) {
            return None;
        }

        // Walk suffixes from longest to shortest; pick the longest matching rule.
        let suffix_len = self.longest_matching_suffix(&labels)?;
        if suffix_len >= labels.len() {
            // host *is* a public suffix.
            return None;
        }
        let start = labels.len() - suffix_len - 1;
        Some(labels[start..].join("."))
    }

    fn longest_matching_suffix(&self, labels: &[&str]) -> Option<usize> {
        // Exceptions trump everything; the exception "!city.kawasaki.jp"
        // means city.kawasaki.jp is a registrable domain itself, so its
        // public suffix is the rule with the leftmost label stripped:
        // i.e. "kawasaki.jp".
        for len in (1..=labels.len()).rev() {
            let candidate = labels[labels.len() - len..].join(".");
            if self.exceptions.contains(&candidate) {
                return Some(len.saturating_sub(1));
            }
        }

        // Plain rules: longest exact match wins.
        let mut best: Option<usize> = None;
        for len in 1..=labels.len() {
            let candidate = labels[labels.len() - len..].join(".");
            if self.rules.contains(&candidate) {
                best = Some(len);
            }
        }

        // Wildcard rules: "*.foo.bar" matches "X.foo.bar" for any X.
        // The matched suffix length is the wildcard parent length + 1.
        for len in 2..=labels.len() {
            let parent = labels[labels.len() - len + 1..].join(".");
            if self.wildcard_parents.contains(&parent) {
                let matched = len;
                if best.map_or(true, |b| matched > b) {
                    best = Some(matched);
                }
            }
        }

        // Implicit fallback rule: every TLD is a public suffix.
        if best.is_none() && !labels.is_empty() {
            best = Some(1);
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMALL_PSL: &str = r#"
// ===BEGIN ICANN DOMAINS===
com
net
uk
co.uk
ac.uk
jp
*.kawasaki.jp
!city.kawasaki.jp
// ===BEGIN PRIVATE DOMAINS===
s3.amazonaws.com
"#;

    fn psl() -> Psl {
        Psl::parse(SMALL_PSL)
    }

    #[test]
    fn simple_com() {
        assert_eq!(psl().registrable_domain("foo.bar.com").as_deref(), Some("bar.com"));
    }

    #[test]
    fn co_uk() {
        assert_eq!(psl().registrable_domain("foo.bar.co.uk").as_deref(), Some("bar.co.uk"));
    }

    #[test]
    fn ac_uk_separate_from_co_uk() {
        assert_eq!(psl().registrable_domain("foo.bar.ac.uk").as_deref(), Some("bar.ac.uk"));
    }

    #[test]
    fn private_suffix() {
        assert_eq!(
            psl().registrable_domain("bucket.s3.amazonaws.com").as_deref(),
            Some("bucket.s3.amazonaws.com")
        );
    }

    #[test]
    fn wildcard_kawasaki() {
        // "foo.kawasaki.jp" -> public suffix is "foo.kawasaki.jp", so no eTLD+1.
        assert_eq!(psl().registrable_domain("foo.kawasaki.jp"), None);
        // "bar.foo.kawasaki.jp" -> public suffix is "foo.kawasaki.jp",
        // registrable is "bar.foo.kawasaki.jp".
        assert_eq!(
            psl().registrable_domain("bar.foo.kawasaki.jp").as_deref(),
            Some("bar.foo.kawasaki.jp")
        );
    }

    #[test]
    fn exception_city_kawasaki_jp() {
        // "!city.kawasaki.jp" makes city.kawasaki.jp a registrable domain
        // itself (public suffix is "kawasaki.jp").
        assert_eq!(
            psl().registrable_domain("city.kawasaki.jp").as_deref(),
            Some("city.kawasaki.jp")
        );
        assert_eq!(
            psl().registrable_domain("ward.city.kawasaki.jp").as_deref(),
            Some("city.kawasaki.jp")
        );
    }

    #[test]
    fn single_label_returns_none() {
        assert_eq!(psl().registrable_domain("com"), None);
        assert_eq!(psl().registrable_domain("localhost"), None);
    }

    #[test]
    fn trailing_dot_and_case() {
        assert_eq!(psl().registrable_domain("Foo.Bar.COM.").as_deref(), Some("bar.com"));
    }

    #[test]
    fn vendored_psl_resolves_real_domains() {
        let psl = Psl::load(std::path::Path::new("data/public_suffix_list.dat")).unwrap();
        assert_eq!(psl.registrable_domain("mail.google.com").as_deref(), Some("google.com"));
        assert_eq!(psl.registrable_domain("foo.bar.co.uk").as_deref(), Some("bar.co.uk"));
        assert_eq!(psl.registrable_domain("foo.example.de").as_deref(), Some("example.de"));
        assert_eq!(
            psl.registrable_domain("bucket.s3.amazonaws.com").as_deref(),
            Some("bucket.s3.amazonaws.com")
        );
    }
}
