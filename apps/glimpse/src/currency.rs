use crate::{
    config::{self, Paths},
    process,
    utility::Answer,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
#[derive(Clone, Serialize, Deserialize)]
struct Quote {
    date: String,
    rate: f64,
}
#[derive(Serialize, Deserialize)]
struct Snapshot {
    fetched_at: u64,
    rates: BTreeMap<String, Quote>,
}
#[derive(Deserialize)]
struct Published {
    base: String,
    quote: String,
    date: String,
    rate: f64,
}
fn code(s: &str) -> bool {
    s.len() == 3 && s.bytes().all(|b| b.is_ascii_uppercase())
}
fn valid(snapshot: &Snapshot) -> bool {
    snapshot.rates.get("USD").is_some_and(|q| q.rate == 1.0)
        && snapshot.rates.iter().all(|(c, q)| code(c) && config::valid_date(&q.date) && q.rate.is_finite() && q.rate > 0.0)
}
fn snapshot(p: &Paths, cancelled: impl Fn() -> bool) -> Result<(Snapshot, bool)> {
    let path = p.data.join("currency-rates.json");
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let cached: Option<Snapshot> = fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).filter(valid);
    if let Some(ref cache) = cached
        && cache.fetched_at <= now
        && now - cache.fetched_at < 86400
    {
        return Ok((cached.unwrap(), false));
    }
    let retry = p.state.join("currency-retry-at");
    let retry_at = fs::read_to_string(&retry).ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    if retry_at > now
        && let Some(cache) = cached
    {
        return Ok((cache, true));
    }
    let fetched = (|| -> Result<Snapshot> {
        // Only the fixed public rate table is requested; amounts and search text stay local.
        let mut command = Command::new("curl");
        command.args(["-fLsS", "--connect-timeout", "3", "--max-time", "8", "https://api.frankfurter.dev/v2/rates?base=USD"]);
        let bytes = process::capture_cancel(command, vec![], Duration::from_secs(9), 512 * 1024, &cancelled)?;
        let published: Vec<Published> = serde_json::from_slice(&bytes)?;
        let date = published.iter().map(|r| r.date.as_str()).max().context("Empty currency rate feed")?.to_string();
        let mut rates = BTreeMap::new();
        for r in published {
            if r.base != "USD" || !code(&r.quote) || !config::valid_date(&r.date) || !r.rate.is_finite() || r.rate <= 0.0 {
                bail!("Invalid currency rate feed");
            }
            rates.insert(r.quote, Quote { date: r.date, rate: r.rate });
        }
        rates.insert("USD".into(), Quote { date, rate: 1.0 });
        let snapshot = Snapshot { fetched_at: now, rates };
        config::atomic_write(&path, &serde_json::to_vec(&snapshot)?)?;
        Ok(snapshot)
    })();
    if cancelled() {
        bail!("operation cancelled");
    }
    match fetched {
        Ok(s) => Ok((s, false)),
        Err(_) => {
            let _ = config::atomic_write(&retry, (now + 300).to_string().as_bytes());
            cached.map(|s| (s, true)).context("Currency rates unavailable. Connect to the internet and try again.")
        }
    }
}
pub fn convert(p: &Paths, value: f64, from: &str, to: &str, cancelled: impl Fn() -> bool) -> Result<Answer> {
    if !value.is_finite() || !code(from) || !code(to) {
        bail!("Invalid currency conversion");
    }
    if from == to {
        let copy = format!("{value:.2} {to}");
        return Ok(Answer {
            title: format!("{} {from} = {copy}", crate::utility::number(value)),
            copy,
            detail: "Same currency · Enter to copy".into(),
        });
    }
    let (snapshot, cached) = snapshot(p, cancelled)?;
    let a = snapshot.rates.get(from).context("Unsupported source currency")?;
    let b = snapshot.rates.get(to).context("Unsupported target currency")?;
    let converted = value * b.rate / a.rate;
    if !converted.is_finite() {
        bail!("Conversion is too large");
    }
    let date = std::cmp::min(&a.date, &b.date);
    let copy = format!("{converted:.2} {to}");
    Ok(Answer {
        title: format!("{} {from} = {copy}", crate::utility::number(value)),
        copy,
        detail: format!("{} · {date} · Frankfurter", if cached { "Cached rates" } else { "Daily rates" }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fresh_cache_converts_locally_with_published_date() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths { config: dir.path().join("config"), data: dir.path().join("data"), state: dir.path().join("state") };
        p.ensure().unwrap();
        let quote = Quote { date: "2026-10-13".into(), rate: 1.0 };
        let snapshot = Snapshot {
            fetched_at: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
            rates: BTreeMap::from([("USD".into(), quote.clone()), ("EUR".into(), Quote { rate: 0.9, ..quote })]),
        };
        config::atomic_write(&p.data.join("currency-rates.json"), &serde_json::to_vec(&snapshot).unwrap()).unwrap();
        let result = convert(&p, 10.0, "USD", "EUR", || false).unwrap();
        assert_eq!(result.copy, "9.00 EUR");
        assert!(result.detail.contains("2026-10-13"));
        let mut snapshot = snapshot;
        snapshot.fetched_at = 0;
        config::atomic_write(&p.data.join("currency-rates.json"), &serde_json::to_vec(&snapshot).unwrap()).unwrap();
        config::atomic_write(&p.state.join("currency-retry-at"), u64::MAX.to_string().as_bytes()).unwrap();
        assert!(convert(&p, 10.0, "USD", "EUR", || false).unwrap().detail.contains("Cached rates"));
    }
}
