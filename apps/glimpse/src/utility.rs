use crate::config::Config;
use anyhow::{Result, bail};

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    depth: usize,
}
impl Parser<'_> {
    fn ws(&mut self) {
        while self.s.get(self.i).is_some_and(u8::is_ascii_whitespace) {
            self.i += 1;
        }
    }
    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        if self.s.get(self.i) == Some(&c) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn expr(&mut self) -> Result<f64> {
        let mut n = self.term()?;
        loop {
            if self.eat(b'+') {
                n += self.term()?;
            } else if self.eat(b'-') {
                n -= self.term()?;
            } else {
                break;
            }
        }
        Ok(n)
    }
    fn term(&mut self) -> Result<f64> {
        let mut n = self.power()?;
        loop {
            if self.eat(b'*') {
                n *= self.power()?;
            } else if self.eat(b'/') {
                n /= self.power()?;
            } else if self.eat(b'%') {
                n %= self.power()?;
            } else {
                break;
            }
        }
        Ok(n)
    }
    fn power(&mut self) -> Result<f64> {
        self.depth += 1;
        if self.depth > 32 {
            bail!("expression too deep");
        }
        let n = if self.eat(b'-') {
            -self.power()?
        } else if self.eat(b'+') {
            self.power()?
        } else {
            let n = self.atom()?;
            if self.eat(b'^') { n.powf(self.power()?) } else { n }
        };
        self.depth -= 1;
        Ok(n)
    }
    fn atom(&mut self) -> Result<f64> {
        if self.eat(b'(') {
            let n = self.expr()?;
            if !self.eat(b')') {
                bail!("missing )");
            }
            return Ok(n);
        }
        self.ws();
        let start = self.i;
        while self.s.get(self.i).is_some_and(|b| b.is_ascii_digit() || *b == b'.') {
            self.i += 1;
        }
        if self.i == start {
            bail!("expected number");
        }
        Ok(std::str::from_utf8(&self.s[start..self.i])?.parse()?)
    }
}
pub fn calculate(s: &str) -> Result<f64> {
    if s.len() > 512 {
        bail!("expression too long");
    }
    let mut p = Parser { s: s.as_bytes(), i: 0, depth: 0 };
    let n = p.expr()?;
    p.ws();
    if p.i != p.s.len() || !n.is_finite() {
        bail!("invalid or nonfinite expression");
    }
    Ok(n)
}
fn unit(s: &str) -> Option<(&'static str, f64, f64)> {
    Some(match s.to_lowercase().as_str() {
        "m" => ("length", 1., 0.),
        "km" => ("length", 1000., 0.),
        "cm" => ("length", 0.01, 0.),
        "mm" => ("length", 0.001, 0.),
        "in" => ("length", 0.0254, 0.),
        "ft" => ("length", 0.3048, 0.),
        "mi" => ("length", 1609.344, 0.),
        "kg" => ("mass", 1., 0.),
        "g" => ("mass", 0.001, 0.),
        "lb" => ("mass", 0.45359237, 0.),
        "oz" => ("mass", 0.028349523125, 0.),
        "s" => ("time", 1., 0.),
        "min" => ("time", 60., 0.),
        "h" => ("time", 3600., 0.),
        "day" => ("time", 86400., 0.),
        "c" | "°c" => ("temperature", 1., 273.15),
        "f" | "°f" => ("temperature", 5. / 9., 255.3722222222222),
        "k" => ("temperature", 1., 0.),
        "l" => ("volume", 1., 0.),
        "ml" => ("volume", 0.001, 0.),
        "floz" => ("volume", 0.0295735295625, 0.),
        "cup" => ("volume", 0.2365882365, 0.),
        "b" => ("data", 1., 0.),
        "kb" => ("data", 1000., 0.),
        "mb" => ("data", 1e6, 0.),
        "gb" => ("data", 1e9, 0.),
        "tb" => ("data", 1e12, 0.),
        "kib" => ("data", 1024., 0.),
        "mib" => ("data", 1048576., 0.),
        "gib" => ("data", 1073741824., 0.),
        _ => return None,
    })
}
pub fn evaluate(q: &str, c: &Config) -> Result<Option<String>> {
    let parts: Vec<_> = q.split_whitespace().collect();
    if parts.len() == 4 && ["to", "in"].contains(&parts[2]) {
        let value: f64 = parts[0].parse()?;
        if !value.is_finite() {
            bail!("nonfinite value");
        }
        if let (Some((d, a, b)), Some((e, x, y))) = (unit(parts[1]), unit(parts[3])) {
            if d != e {
                bail!("incompatible units");
            }
            let base = value * a + b;
            if d == "temperature" && base < 0. {
                bail!("below absolute zero");
            }
            let n = (base - y) / x;
            if !n.is_finite() {
                bail!("overflow");
            }
            return Ok(Some(format!("{n:.8} {}", parts[3])));
        }
        let from = parts[1].to_uppercase();
        let to = parts[3].to_uppercase();
        if from.len() == 3 && to.len() == 3 {
            let (Some(a), Some(b)) = (c.rates.get(&from), c.rates.get(&to)) else {
                bail!("missing supplied currency rates");
            };
            return Ok(Some(format!("{:.2} {to} · rates {}", value * b / a, c.rates_date)));
        }
    }
    let s = q.trim().strip_prefix('=').unwrap_or(q.trim());
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_whitespace() || b".+-*/%^()".contains(&b)) {
        return calculate(s).map(|n| Some(format!("{n}")));
    }
    Ok(None)
}

#[derive(Clone, Debug, PartialEq)]
pub enum Intent {
    Search,
    Calculation,
    Conversion,
}
#[derive(Clone, Debug)]
pub struct Answer {
    pub title: String,
    pub copy: String,
    pub detail: String,
}
pub fn normalize_math(q: &str) -> String {
    q.trim()
        .strip_suffix('=')
        .unwrap_or(q.trim())
        .trim()
        .trim_start_matches('=')
        .replace('×', "*")
        .replace('÷', "/")
        .replace('−', "-")
}
fn conversion_parts(q: &str) -> Vec<String> {
    let expanded = q.to_lowercase().replace("fluid ounces", "floz").replace("fluid ounce", "floz").replace("fl oz", "floz");
    let q = expanded.trim().strip_suffix('=').unwrap_or(expanded.trim()).trim();
    // Split an attached quantity (1oz), while retaining the signs and decimal point.
    let boundary = q.char_indices().find(|(_, c)| c.is_alphabetic() || *c == '°').map(|(i, _)| i);
    let spaced = match boundary {
        Some(i) if i > 0 && !q[..i].ends_with(char::is_whitespace) => format!("{} {}", &q[..i], &q[i..]),
        _ => q.into(),
    };
    spaced.split_whitespace().map(str::to_lowercase).collect()
}
fn canonical_unit(s: &str) -> &str {
    match s {
        "meter" | "meters" | "metre" | "metres" => "m",
        "kilometer" | "kilometers" | "kilometre" | "kilometres" => "km",
        "centimeter" | "centimeters" => "cm",
        "millimeter" | "millimeters" => "mm",
        "inch" | "inches" => "in",
        "foot" | "feet" => "ft",
        "mile" | "miles" => "mi",
        "kilogram" | "kilograms" => "kg",
        "gram" | "grams" => "g",
        "pound" | "pounds" | "lbs" => "lb",
        "ounce" | "ounces" => "oz",
        "second" | "seconds" => "s",
        "minute" | "minutes" => "min",
        "hour" | "hours" => "h",
        "days" => "day",
        "celsius" => "c",
        "fahrenheit" => "f",
        "kelvin" => "k",
        "litre" | "litres" | "liter" | "liters" => "l",
        "milliliter" | "milliliters" => "ml",
        "cups" => "cup",
        "byte" | "bytes" => "b",
        _ => s,
    }
}
pub fn intent(q: &str) -> Intent {
    let parts = conversion_parts(q);
    if parts.len() >= 2
        && parts[0].parse::<f64>().is_ok()
        && (unit(canonical_unit(&parts[1])).is_some() || parts.get(2).is_some_and(|p| p == "to" || p == "in"))
    {
        return Intent::Conversion;
    }
    let math = normalize_math(q);
    if q.trim().starts_with('=')
        || (!math.is_empty()
            && math.chars().all(|c| c.is_ascii_digit() || c.is_whitespace() || ".+-*/%^()=".contains(c))
            && (math.chars().any(|c| "+-*/%^()=".contains(c)) || q.trim().ends_with('=')))
    {
        Intent::Calculation
    } else {
        Intent::Search
    }
}
pub fn number(n: f64) -> String {
    let n = if n == 0.0 { 0.0 } else { n };
    if n != 0.0 && (n.abs() < 1e-6 || n.abs() >= 1e12) {
        let scientific = format!("{n:.9e}");
        let (mantissa, exponent) = scientific.split_once('e').unwrap();
        return format!("{}e{exponent}", mantissa.trim_end_matches('0').trim_end_matches('.'));
    }
    let decimals = if n == 0.0 { 0 } else { (9.0 - n.abs().log10().floor()).clamp(0.0, 12.0) as usize };
    let s = format!("{n:.decimals$}");
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').into() } else { s }
}
pub fn pretty_math(q: &str) -> String {
    let mut out = String::new();
    let mut previous = None;
    for ch in normalize_math(q).chars().filter(|c| !c.is_whitespace()) {
        if "+-*/%^".contains(ch) && previous.is_some_and(|c| c != '(' && !"+-*/%^".contains(c)) {
            out.push(' ');
            out.push(ch);
            out.push(' ');
        } else {
            out.push(ch);
        }
        previous = Some(ch);
    }
    out
}
pub fn currency_query(q: &str) -> Option<(f64, String, String)> {
    let parts = conversion_parts(q);
    if intent(q) != Intent::Conversion
        || parts.len() != 4
        || unit(canonical_unit(&parts[1])).is_some()
        || unit(canonical_unit(&parts[3])).is_some()
    {
        return None;
    }
    let from = parts[1].to_uppercase();
    let to = parts[3].to_uppercase();
    if from.len() != 3 || to.len() != 3 || !from.bytes().chain(to.bytes()).all(|b| b.is_ascii_uppercase()) {
        return None;
    }
    Some((parts[0].parse().ok()?, from, to))
}
pub fn answers_local(p: &crate::config::Paths, q: &str, c: &Config, cancelled: impl Fn() -> bool) -> Result<Vec<Answer>> {
    if let Some((value, from, to)) = currency_query(q) {
        return Ok(vec![crate::currency::convert(p, value, &from, &to, cancelled)?]);
    }
    answers(q, c)
}
pub fn answers(q: &str, c: &Config) -> Result<Vec<Answer>> {
    match intent(q) {
        Intent::Search => Ok(vec![]),
        Intent::Calculation => {
            let n = number(calculate(&normalize_math(q))?);
            Ok(vec![Answer { title: format!("{} = {n}", pretty_math(q)), copy: n, detail: "Enter to copy answer".into() }])
        }
        Intent::Conversion => {
            let p = conversion_parts(q);
            if p.len() != 4 || !["to", "in"].contains(&p[2].as_str()) {
                bail!("Complete the conversion, for example: 1 oz to lb");
            }
            let value: f64 = p[0].parse()?;
            if !value.is_finite() {
                bail!("Use a finite quantity");
            }
            let from = canonical_unit(&p[1]);
            let to = canonical_unit(&p[3]);
            if let (Some((dimension, factor, offset)), Some((target_dimension, _, _))) = (unit(from), unit(to)) {
                if dimension != target_dimension {
                    bail!("These units measure different things");
                }
                let base = value * factor + offset;
                if dimension == "temperature" && base < 0.0 {
                    bail!("Temperature cannot be below absolute zero");
                }
                let related: &[&str] = match dimension {
                    "mass" => &["kg", "g", "lb", "oz"],
                    "length" => &["m", "km", "cm", "mm", "in", "ft", "mi"],
                    "time" => &["s", "min", "h", "day"],
                    "volume" => &["l", "ml", "floz", "cup"],
                    "temperature" => &["c", "f", "k"],
                    "data" => &["b", "kb", "mb", "gb", "tb", "kib", "mib", "gib"],
                    _ => &[],
                };
                let mut targets = vec![to];
                targets.extend(related.iter().copied().filter(|u| *u != from && *u != to));
                let mut result = Vec::new();
                for (i, target) in targets.into_iter().enumerate() {
                    let (_, factor, offset) = unit(target).unwrap();
                    let answer = (base - offset) / factor;
                    let answer = if dimension == "temperature" && answer.abs() < 1e-10 { 0.0 } else { answer };
                    if !answer.is_finite() {
                        bail!("Conversion is too large");
                    }
                    let copy = format!("{} {target}", number(answer));
                    result.push(Answer {
                        title: format!("{} {from} = {copy}", number(value)),
                        copy,
                        detail: if i == 0 {
                            "Requested conversion · Enter to copy"
                        } else {
                            "Related conversion · Enter to copy"
                        }
                        .into(),
                    });
                }
                Ok(result)
            } else {
                let from = from.to_uppercase();
                let to = to.to_uppercase();
                let (Some(a), Some(b)) = (c.rates.get(&from), c.rates.get(&to)) else {
                    bail!("Unknown or incompatible conversion units");
                };
                let converted = value * b / a;
                if !converted.is_finite() {
                    bail!("Conversion is too large");
                }
                let copy = format!("{converted:.2} {to}");
                Ok(vec![Answer {
                    title: format!("{} {from} = {copy}", number(value)),
                    copy,
                    detail: format!("Supplied rates · {}", c.rates_date),
                }])
            }
        }
    }
}
