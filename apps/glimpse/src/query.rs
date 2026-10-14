/// Shared lexical parsing keeps numeric terms exact and phrases together.
#[derive(Debug)]
struct Term {
    text: String,
    phrase: bool,
}
fn terms(q: &str) -> Vec<Term> {
    let mut result = Vec::new();
    let mut text = String::new();
    let mut quoted = false;
    for c in q.chars() {
        if c == '"' {
            if !text.is_empty() {
                result.push(Term { text: std::mem::take(&mut text).to_lowercase(), phrase: quoted });
            }
            quoted = !quoted;
        } else if quoted || c.is_alphanumeric() {
            text.push(c);
        } else if !text.is_empty() {
            result.push(Term { text: std::mem::take(&mut text).to_lowercase(), phrase: false });
        }
    }
    if !text.is_empty() {
        result.push(Term { text: text.to_lowercase(), phrase: quoted });
    }
    result.truncate(16);
    result
}
pub fn fts(q: &str) -> String {
    let terms = terms(q);
    let last = terms.len().saturating_sub(1);
    terms
        .iter()
        .enumerate()
        .map(|(i, t)| {
            format!(
                "\"{}\"{}",
                t.text.replace('"', "\"\""),
                if i == last && !t.phrase && !t.text.chars().all(char::is_numeric) { "*" } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}
/// For searching contents: every word exactly (no prefix), and when there are several,
/// within a few words of each other.
pub fn fts_near(q: &str) -> String {
    let terms: Vec<String> = terms(q).iter().map(|t| format!("\"{}\"", t.text.replace('"', "\"\""))).collect();
    match terms.len() {
        0 => String::new(),
        1 => terms[0].clone(),
        _ => format!("NEAR({}, 5)", terms.join(" ")),
    }
}
pub fn name_rank(name: &str, q: &str) -> Option<u8> {
    let name = name.to_lowercase();
    let requested = terms(q);
    let q = q.trim().trim_matches('"').to_lowercase();
    if requested.is_empty() {
        return None;
    }
    let words: Vec<_> = name.split(|c: char| !c.is_alphanumeric()).filter(|s| !s.is_empty()).collect();
    let last = requested.len() - 1;
    if !requested.iter().enumerate().all(|(i, t)| {
        if t.phrase {
            let phrase: Vec<_> = t.text.split(|c: char| !c.is_alphanumeric()).filter(|s| !s.is_empty()).collect();
            return !phrase.is_empty() && words.windows(phrase.len()).any(|window| window == phrase);
        }
        words.iter().any(|w| *w == t.text || (i == last && !t.text.chars().all(char::is_numeric) && w.starts_with(&t.text)))
    }) {
        return None;
    }
    Some(if name == q {
        0
    } else if name.starts_with(&q) {
        1
    } else {
        2
    })
}
pub fn semantic_allowed(q: &str) -> bool {
    q.trim().starts_with("semantic:")
        || q.trim().starts_with("code:")
        || terms(q).iter().filter(|t| t.text.chars().any(char::is_alphabetic)).count() >= 2
}
