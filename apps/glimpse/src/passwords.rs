//! Reading a `pw …` query (Nixe Passwords). A query whose first word is `pw` or
//! `password` belongs to the vault alone: no other provider, plugin or semantic search
//! sees it.

/// What Enter does to a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Username,
    Password,
    Totp,
    Open,
}

impl Verb {
    /// The daemon's field name, for the copying verbs.
    pub fn field(self) -> Option<&'static str> {
        match self {
            Verb::Username => Some("username"),
            Verb::Password => Some("password"),
            Verb::Totp => Some("totp"),
            Verb::Open => None,
        }
    }

    /// What Enter does, for the status bar (short: the bar holds several).
    pub fn label(self) -> &'static str {
        match self {
            Verb::Username => "Username",
            Verb::Password => "Password",
            Verb::Totp => "Code",
            Verb::Open => "Open site",
        }
    }
}

/// `pw generate` and `pw lock`, offered above any items that match the word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Special {
    Generate,
    Lock,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    /// What to search for; empty means suggestions (recent items, the active site).
    pub terms: String,
    pub verb: Verb,
    pub special: Option<Special>,
}

/// A `pw` query, or `None` for anything else.
pub fn parse(text: &str) -> Option<Query> {
    let text = text.trim_start();
    let (first, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    if !(first.eq_ignore_ascii_case("pw") || first.eq_ignore_ascii_case("password")) {
        return None;
    }
    let words: Vec<&str> = rest.split_whitespace().collect();
    let special = match words.as_slice() {
        [w] if w.eq_ignore_ascii_case("generate") => Some(Special::Generate),
        [w] if w.eq_ignore_ascii_case("lock") => Some(Special::Lock),
        _ => None,
    };
    // A trailing verb counts only after other words: `pw pass` searches for "pass".
    let verb = match words.as_slice() {
        [_, .., last] => match last.to_ascii_lowercase().as_str() {
            "user" | "username" => Some(Verb::Username),
            "pass" | "password" => Some(Verb::Password),
            "otp" | "code" | "totp" => Some(Verb::Totp),
            "open" => Some(Verb::Open),
            _ => None,
        },
        _ => None,
    };
    let terms = match verb {
        Some(_) => words[..words.len() - 1].join(" "),
        None => words.join(" "),
    };
    Some(Query { terms, verb: verb.unwrap_or(Verb::Username), special })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(terms: &str, verb: Verb, special: Option<Special>) -> Option<Query> {
        Some(Query { terms: terms.into(), verb, special })
    }

    #[test]
    fn prefixes() {
        assert_eq!(parse("pw"), q("", Verb::Username, None));
        assert_eq!(parse("  PW  "), q("", Verb::Username, None));
        assert_eq!(parse("password github"), q("github", Verb::Username, None));
        assert_eq!(parse("pwd github"), None);
        assert_eq!(parse("pwgen"), None);
        assert_eq!(parse("passwords"), None);
        assert_eq!(parse("github pw"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn verbs() {
        assert_eq!(parse("pw github"), q("github", Verb::Username, None));
        assert_eq!(parse("pw github user"), q("github", Verb::Username, None));
        assert_eq!(parse("pw github pass"), q("github", Verb::Password, None));
        assert_eq!(parse("pw github otp"), q("github", Verb::Totp, None));
        assert_eq!(parse("pw github open"), q("github", Verb::Open, None));
        assert_eq!(parse("pw work github PASS"), q("work github", Verb::Password, None));
        // A lone verb word is a search term.
        assert_eq!(parse("pw pass"), q("pass", Verb::Username, None));
        assert_eq!(parse("pw open"), q("open", Verb::Username, None));
    }

    #[test]
    fn specials() {
        assert_eq!(parse("pw generate"), q("generate", Verb::Username, Some(Special::Generate)));
        assert_eq!(parse("pw Lock"), q("Lock", Verb::Username, Some(Special::Lock)));
        assert_eq!(parse("pw lock screen"), q("lock screen", Verb::Username, None));
        assert_eq!(parse("pw generate pass"), q("generate", Verb::Password, None));
    }
}
