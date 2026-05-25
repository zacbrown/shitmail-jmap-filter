/// Parses RFC 8601 Authentication-Results headers and extracts the DKIM
/// signing domain (`header.d=`) from a passing DKIM verdict whose
/// `authserv-id` matches a trusted set.
///
/// Sender-added Authentication-Results headers are ignored by the
/// trusted-authserv-id filter; only headers added by our own mail server
/// are believed.
///
/// When multiple passing DKIM verdicts exist, prefers one whose signing
/// domain aligns with the `From:` registrable domain.
pub struct Verdict {
    pub signing_domain: String,
    pub aligned: bool,
}

pub fn extract_verified_domain(
    headers: &[String],
    trusted_authserv_ids: &[String],
    from_registrable: Option<&str>,
    eltd1: impl Fn(&str) -> Option<String>,
) -> Option<Verdict> {
    let mut best: Option<Verdict> = None;

    for raw in headers {
        let Some((authserv, methods)) = parse_authres(raw) else {
            continue;
        };
        if !trusted_authserv_ids
            .iter()
            .any(|t| authserv_matches(t, &authserv))
        {
            continue;
        }
        for m in methods {
            if !m.method.eq_ignore_ascii_case("dkim") || !m.result.eq_ignore_ascii_case("pass") {
                continue;
            }
            let Some(d) = m.header_d else { continue };
            let d = d.trim().trim_matches('"').to_ascii_lowercase();
            if d.is_empty() {
                continue;
            }
            let aligned = match from_registrable {
                Some(from_reg) => match eltd1(&d) {
                    Some(d_reg) => d_reg.eq_ignore_ascii_case(from_reg),
                    None => false,
                },
                None => false,
            };
            let candidate = Verdict { signing_domain: d, aligned };
            match &best {
                None => best = Some(candidate),
                Some(b) if !b.aligned && candidate.aligned => best = Some(candidate),
                _ => {}
            }
        }
    }

    best
}

fn authserv_matches(trusted: &str, observed: &str) -> bool {
    let t = trusted.to_ascii_lowercase();
    let o = observed.to_ascii_lowercase();
    o == t || o.ends_with(&format!(".{}", t))
}

struct MethodResult {
    method: String,
    result: String,
    header_d: Option<String>,
}

/// Parses a single Authentication-Results header per RFC 8601 §2.2 into
/// the authserv-id and the list of method/result entries.
fn parse_authres(raw: &str) -> Option<(String, Vec<MethodResult>)> {
    let cleaned = unfold(raw);
    let mut parts = cleaned.split(';');
    let head = parts.next()?.trim().to_string();
    let authserv = head.split_ascii_whitespace().next()?.to_string();

    let mut methods = Vec::new();
    for entry in parts {
        let entry = entry.trim();
        if entry.is_empty() || entry.eq_ignore_ascii_case("none") {
            continue;
        }

        let tokens = tokenize(entry);
        if tokens.is_empty() {
            continue;
        }
        let (method, result) = match tokens[0].split_once('=') {
            Some((m, r)) => (m.trim().to_string(), r.trim().to_string()),
            None => continue,
        };

        let mut header_d = None;
        for tok in &tokens[1..] {
            if let Some((k, v)) = tok.split_once('=') {
                let key = k.trim();
                let val = v.trim();
                if key.eq_ignore_ascii_case("header.d") {
                    header_d = Some(strip_quotes(val).to_string());
                }
            }
        }
        methods.push(MethodResult { method, result, header_d });
    }

    Some((authserv, methods))
}

fn unfold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch == '\r' || ch == '\n' {
            if !out.ends_with(' ') {
                out.push(' ');
            }
        } else if ch == '\t' {
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
}

fn strip_quotes(s: &str) -> &str {
    let t = s.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        &t[1..t.len() - 1]
    } else {
        t
    }
}

/// Splits an A-R "entry" into whitespace-separated tokens, but keeps
/// quoted strings and parenthesised comments intact. Drops comments.
fn tokenize(entry: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut paren_depth: i32 = 0;
    let mut chars = entry.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            cur.push(c);
            if c == '"' {
                in_quotes = false;
            }
        } else if paren_depth > 0 {
            if c == '(' {
                paren_depth += 1;
            } else if c == ')' {
                paren_depth -= 1;
            }
        } else if c == '"' {
            cur.push(c);
            in_quotes = true;
        } else if c == '(' {
            paren_depth += 1;
        } else if c.is_whitespace() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn psl(d: &str) -> Option<String> {
        let parts: Vec<&str> = d.split('.').collect();
        if parts.len() >= 2 {
            Some(parts[parts.len() - 2..].join("."))
        } else {
            None
        }
    }

    fn trusted() -> Vec<String> {
        vec!["fastmail.com".into(), "messagingengine.com".into()]
    }

    #[test]
    fn aligned_pass_is_picked() {
        let h = vec!["mx1.messagingengine.com; dkim=pass header.d=stripe.com header.i=@stripe.com; spf=pass smtp.mailfrom=stripe.com".to_string()];
        let v = extract_verified_domain(&h, &trusted(), Some("stripe.com"), psl).unwrap();
        assert_eq!(v.signing_domain, "stripe.com");
        assert!(v.aligned);
    }

    #[test]
    fn aligned_preferred_over_unaligned() {
        let h = vec!["mx1.messagingengine.com; dkim=pass header.d=mandrillapp.com; dkim=pass header.d=stripe.com header.i=@stripe.com".to_string()];
        let v = extract_verified_domain(&h, &trusted(), Some("stripe.com"), psl).unwrap();
        assert_eq!(v.signing_domain, "stripe.com");
        assert!(v.aligned);
    }

    #[test]
    fn untrusted_authserv_ignored() {
        let h = vec!["evil.example; dkim=pass header.d=stripe.com".to_string()];
        assert!(extract_verified_domain(&h, &trusted(), Some("stripe.com"), psl).is_none());
    }

    #[test]
    fn dkim_fail_ignored() {
        let h = vec!["mx1.messagingengine.com; dkim=fail header.d=stripe.com".to_string()];
        assert!(extract_verified_domain(&h, &trusted(), Some("stripe.com"), psl).is_none());
    }

    #[test]
    fn no_dkim_returns_none() {
        let h = vec!["mx1.messagingengine.com; spf=pass smtp.mailfrom=stripe.com".to_string()];
        assert!(extract_verified_domain(&h, &trusted(), Some("stripe.com"), psl).is_none());
    }

    #[test]
    fn folded_header_parsed() {
        let h = vec!["mx1.messagingengine.com;\r\n  dkim=pass\r\n    header.d=stripe.com".to_string()];
        let v = extract_verified_domain(&h, &trusted(), None, psl).unwrap();
        assert_eq!(v.signing_domain, "stripe.com");
    }

    #[test]
    fn subdomain_authserv_id_accepted() {
        let h = vec!["mx5.messagingengine.com; dkim=pass header.d=stripe.com".to_string()];
        let v = extract_verified_domain(&h, &trusted(), None, psl).unwrap();
        assert_eq!(v.signing_domain, "stripe.com");
    }
}
