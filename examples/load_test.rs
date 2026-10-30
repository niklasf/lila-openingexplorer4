// Replays requests from a log (one path with query string per line, plain or
// zstd compressed) against a running explorer server, and reports the
// throughput, latency percentiles and response validity.
//
// usage: load_test <log> <base url> [--prefix /lichess] [--concurrency 32]
//        [--limit N] [--skip N] [--warmup N] [--unique]
//
// Lines with other prefixes are skipped. With --unique, repeated requests
// are skipped (to measure the uncached path).

use std::{
    collections::HashSet,
    env, fs,
    io::{self, BufRead, BufReader, Read},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use serde_json::Value;
use tokio::{sync::Mutex, task::JoinSet};

struct Args {
    log: String,
    base: String,
    prefix: String,
    concurrency: usize,
    limit: usize,
    skip: usize,
    warmup: usize,
    unique: bool,
}

fn parse_args() -> Args {
    let mut args = env::args().skip(1);
    let log = args.next().expect("log path");
    let base = args.next().expect("base url");
    let mut parsed = Args {
        log,
        base,
        prefix: "/lichess".to_owned(),
        concurrency: 32,
        limit: usize::MAX,
        skip: 0,
        warmup: 0,
        unique: false,
    };
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--prefix" => parsed.prefix = args.next().expect("prefix"),
            "--concurrency" => parsed.concurrency = args.next().expect("n").parse().expect("n"),
            "--limit" => parsed.limit = args.next().expect("n").parse().expect("n"),
            "--skip" => parsed.skip = args.next().expect("n").parse().expect("n"),
            "--warmup" => parsed.warmup = args.next().expect("n").parse().expect("n"),
            "--unique" => parsed.unique = true,
            _ => panic!("unknown flag {flag}"),
        }
    }
    parsed
}

fn read_requests(args: &Args) -> io::Result<Vec<String>> {
    let file = fs::File::open(&args.log)?;
    let reader: Box<dyn Read> = if args.log.ends_with(".zst") {
        Box::new(zstd::Decoder::new(file)?)
    } else {
        Box::new(file)
    };
    let mut seen = HashSet::new();
    let mut requests = Vec::new();
    for line in BufReader::new(reader).lines() {
        let line = line?;
        let path = line.split('?').next().unwrap_or("");
        if path != args.prefix {
            continue;
        }
        if args.unique && !seen.insert(line.clone()) {
            continue;
        }
        requests.push(line);
    }
    let end = args.skip.saturating_add(args.limit).min(requests.len());
    Ok(requests[args.skip.min(end)..end].to_vec())
}

#[derive(Default)]
struct Outcome {
    latencies: Vec<Duration>,
    statuses: [usize; 6],
    invalid: usize,
    bytes: usize,
    examples: Vec<String>,
}

/// Checks the structure of an explorer response.
fn validate(body: &[u8]) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body).map_err(|err| err.to_string())?;
    let obj = value.as_object().ok_or("not an object")?;
    let count = |obj: &serde_json::Map<String, Value>, key: &str| -> Result<u64, String> {
        obj.get(key)
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("missing {key}"))
    };
    let total = count(obj, "white")? + count(obj, "draws")? + count(obj, "black")?;
    let moves = obj
        .get("moves")
        .and_then(Value::as_array)
        .ok_or("missing moves")?;
    let mut sum = 0;
    for m in moves {
        let m = m.as_object().ok_or("move not an object")?;
        sum += count(m, "white")? + count(m, "draws")? + count(m, "black")?;
        m.get("uci").and_then(Value::as_str).ok_or("missing uci")?;
        let san = m.get("san").and_then(Value::as_str).ok_or("missing san")?;
        if san == "--" {
            return Err("null san".to_owned());
        }
    }
    if sum > total {
        return Err(format!("moves sum {sum} > total {total}"));
    }
    if !obj.contains_key("opening") {
        return Err("missing opening".to_owned());
    }
    for key in ["topGames", "recentGames"] {
        if let Some(games) = obj.get(key) {
            for g in games.as_array().ok_or("games not an array")? {
                let g = g.as_object().ok_or("game not an object")?;
                g.get("id").and_then(Value::as_str).ok_or("missing id")?;
                g.get("uci").and_then(Value::as_str).ok_or("missing uci")?;
                for color in ["white", "black"] {
                    g.get(color)
                        .and_then(|p| p.get("name"))
                        .and_then(Value::as_str)
                        .ok_or("missing player name")?;
                }
            }
        }
    }
    Ok(())
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let i = ((sorted.len() as f64 * p).ceil() as usize).clamp(1, sorted.len()) - 1;
    sorted[i]
}

#[tokio::main]
async fn main() {
    let args = parse_args();
    let requests = Arc::new(read_requests(&args).expect("read log"));
    let total = requests.len();
    eprintln!(
        "{total} requests with prefix {} ({} warmup), concurrency {}",
        args.prefix, args.warmup, args.concurrency
    );
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(args.concurrency)
        .timeout(Duration::from_secs(60))
        .build()
        .expect("client");
    let next = Arc::new(AtomicUsize::new(0));
    let outcome = Arc::new(Mutex::new(Outcome::default()));
    let started = Instant::now();
    let warmup = args.warmup;
    let mut tasks = JoinSet::new();
    for _ in 0..args.concurrency {
        let requests = Arc::clone(&requests);
        let next = Arc::clone(&next);
        let outcome = Arc::clone(&outcome);
        let client = client.clone();
        let base = args.base.clone();
        tasks.spawn(async move {
            let mut local = Outcome::default();
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= requests.len() {
                    break;
                }
                let url = format!("{base}{}", requests[i]);
                let t = Instant::now();
                let (status, body) = match client.get(&url).send().await {
                    Ok(resp) => {
                        let status = resp.status().as_u16();
                        let body = resp.bytes().await.unwrap_or_default();
                        (status, body)
                    }
                    Err(err) => {
                        if local.examples.len() < 5 {
                            local.examples.push(format!("{url}: {err}"));
                        }
                        (0, Default::default())
                    }
                };
                let elapsed = t.elapsed();
                if i < warmup {
                    continue;
                }
                local.latencies.push(elapsed);
                local.statuses[(status / 100) as usize % 6] += 1;
                local.bytes += body.len();
                if status == 200 {
                    if let Err(err) = validate(&body) {
                        local.invalid += 1;
                        if local.examples.len() < 5 {
                            local.examples.push(format!("{url}: {err}"));
                        }
                    }
                } else if local.examples.len() < 5 {
                    local.examples.push(format!(
                        "{url}: {status} {}",
                        String::from_utf8_lossy(&body[..body.len().min(200)])
                    ));
                }
            }
            let mut outcome = outcome.lock().await;
            outcome.latencies.append(&mut local.latencies);
            for (a, b) in outcome.statuses.iter_mut().zip(local.statuses) {
                *a += b;
            }
            outcome.invalid += local.invalid;
            outcome.bytes += local.bytes;
            outcome.examples.extend(local.examples);
        });
    }
    while tasks.join_next().await.is_some() {}
    let elapsed = started.elapsed();
    let mut outcome = Arc::try_unwrap(outcome)
        .ok()
        .expect("tasks done")
        .into_inner();
    outcome.latencies.sort();
    let n = outcome.latencies.len();
    let mean = outcome.latencies.iter().sum::<Duration>() / n.max(1) as u32;
    println!(
        "{n} requests in {elapsed:.1?}: {:.0} req/s, {:.1} MB; statuses 2xx {} 4xx {} 5xx {} err {}; invalid {}",
        n as f64 / elapsed.as_secs_f64(),
        outcome.bytes as f64 / 1e6,
        outcome.statuses[2],
        outcome.statuses[4],
        outcome.statuses[5],
        outcome.statuses[0],
        outcome.invalid,
    );
    println!(
        "latency mean {mean:.2?}, p50 {:.2?}, p90 {:.2?}, p99 {:.2?}, p99.9 {:.2?}, max {:.2?}",
        percentile(&outcome.latencies, 0.5),
        percentile(&outcome.latencies, 0.9),
        percentile(&outcome.latencies, 0.99),
        percentile(&outcome.latencies, 0.999),
        outcome.latencies.last().copied().unwrap_or_default(),
    );
    for example in &outcome.examples {
        println!("example: {example}");
    }
}
