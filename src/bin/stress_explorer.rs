use std::{
    collections::HashSet,
    fs,
    io::{self, BufRead, BufReader, Read},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use clap::{ArgGroup, Parser};
use lila_openingexplorer4::packed::GameId;
use serde_json::{Map, Value};
use tokio::task::JoinSet;

/// Replays requests from a log against running explorer servers, and
/// reports throughput, latency percentiles and invalid responses.
///
/// The log has one path with query string per line, like
/// `/lichess?fen=...`, and may be zstd compressed (`.zst`). Requests are
/// sent in the order of the log. Requests that do not match a route of
/// the given servers are skipped.
#[derive(Parser)]
#[command(group(ArgGroup::new("servers").required(true).multiple(true)))]
struct Args {
    /// Request log.
    log: PathBuf,
    /// Base URL of the lichess explorer, for /lichess and /lichess/history.
    #[arg(long, group = "servers")]
    lichess_url: Option<String>,
    /// Base URL of the masters explorer, for /masters and
    /// /masters/pgn/{id} (also /master).
    #[arg(long, group = "servers")]
    masters_url: Option<String>,
    /// Concurrent requests, across all targets.
    #[arg(long, default_value = "32")]
    concurrency: usize,
    /// Skip the first requests of the log.
    #[arg(long, default_value = "0")]
    skip: usize,
    /// Replay at most this many requests (after warm-up).
    #[arg(long)]
    limit: Option<usize>,
    /// Requests to send before measuring, for example to fill caches.
    #[arg(long, default_value = "0")]
    warm_up: usize,
    /// Skip repeated requests, to measure responses that are not in the
    /// response cache.
    #[arg(long)]
    unique: bool,
    /// Timeout of each request in seconds.
    #[arg(long, default_value = "60")]
    timeout: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Lichess,
    Masters,
}

impl Kind {
    /// The server for the path, if it is one of the routes.
    fn route(path: &str) -> Option<Kind> {
        match path {
            "/lichess" | "/lichess/history" => Some(Kind::Lichess),
            "/masters" | "/master" => Some(Kind::Masters),
            _ => path
                .strip_prefix("/masters/pgn/")
                .or_else(|| path.strip_prefix("/master/pgn/"))
                .filter(|id| id.parse::<GameId>().is_ok())
                .map(|_| Kind::Masters),
        }
    }
}

struct Target {
    kind: Kind,
    name: &'static str,
    url: String,
}

struct Request {
    target: usize,
    /// Path with query string.
    path: String,
}

/// The requests for the targets, and the number of skipped lines.
fn read_requests(args: &Args, targets: &[Target]) -> io::Result<(Vec<Request>, usize)> {
    let file = fs::File::open(&args.log)?;
    let reader: Box<dyn Read> = if args.log.extension().is_some_and(|ext| ext == "zst") {
        Box::new(zstd::Decoder::new(file)?)
    } else {
        Box::new(file)
    };
    let mut seen = HashSet::new();
    let mut requests = Vec::new();
    let mut skipped = 0;
    for line in BufReader::new(reader).lines() {
        let line = line?;
        let path = line.split_once('?').map_or(line.as_str(), |(path, _)| path);
        let Some(target) = Kind::route(path)
            .and_then(|kind| targets.iter().position(|target| target.kind == kind))
        else {
            skipped += 1;
            continue;
        };
        if args.unique && !seen.insert(line.clone()) {
            continue;
        }
        requests.push(Request { target, path: line });
    }
    let requests = requests
        .into_iter()
        .skip(args.skip)
        .take(
            args.warm_up
                .saturating_add(args.limit.unwrap_or(usize::MAX)),
        )
        .collect();
    Ok((requests, skipped))
}

/// Most examples of failed requests to report.
const MAX_EXAMPLES: usize = 5;

#[derive(Default, Clone)]
struct Outcome {
    latencies: Vec<Duration>,
    ok: usize,
    client_errors: usize,
    server_errors: usize,
    other_statuses: usize,
    /// Requests that failed without a response.
    failed: usize,
    /// Successful responses that are not valid explorer responses.
    invalid: usize,
    bytes: usize,
    examples: Vec<String>,
}

impl Outcome {
    fn example(&mut self, example: String) {
        if self.examples.len() < MAX_EXAMPLES {
            self.examples.push(example);
        }
    }

    fn merge(&mut self, other: &Outcome) {
        self.latencies.extend_from_slice(&other.latencies);
        self.ok += other.ok;
        self.client_errors += other.client_errors;
        self.server_errors += other.server_errors;
        self.other_statuses += other.other_statuses;
        self.failed += other.failed;
        self.invalid += other.invalid;
        self.bytes += other.bytes;
        for example in &other.examples {
            self.example(example.clone());
        }
    }
}

fn count(obj: &Map<String, Value>, key: &str) -> Result<u64, String> {
    obj.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("missing {key}"))
}

fn stats(obj: &Map<String, Value>) -> Result<u64, String> {
    Ok(count(obj, "white")? + count(obj, "draws")? + count(obj, "black")?)
}

/// Checks the structure of an explorer response.
fn validate(body: &[u8]) -> Result<(), String> {
    let value: Value = serde_json::from_slice(body).map_err(|err| err.to_string())?;
    let obj = value.as_object().ok_or("not an object")?;
    let total = stats(obj)?;
    let moves = obj
        .get("moves")
        .and_then(Value::as_array)
        .ok_or("missing moves")?;
    let mut sum = 0;
    for m in moves {
        let m = m.as_object().ok_or("move not an object")?;
        sum += stats(m)?;
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
        let Some(games) = obj.get(key) else {
            continue;
        };
        for game in games.as_array().ok_or("games not an array")? {
            let game = game.as_object().ok_or("game not an object")?;
            game.get("id").and_then(Value::as_str).ok_or("missing id")?;
            game.get("uci")
                .and_then(Value::as_str)
                .ok_or("missing uci")?;
            for color in ["white", "black"] {
                game.get(color)
                    .and_then(|player| player.get("name"))
                    .and_then(Value::as_str)
                    .ok_or("missing player name")?;
            }
        }
    }
    Ok(())
}

async fn request(client: &reqwest::Client, url: &str, outcome: &mut Outcome) {
    let started = Instant::now();
    let response = match client.get(url).send().await {
        Ok(response) => response,
        Err(err) => {
            outcome.failed += 1;
            outcome.example(format!("{url}: {err}"));
            return;
        }
    };
    let status = response.status();
    let is_json = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .is_some_and(|value| value.as_bytes().starts_with(b"application/json"));
    let body = match response.bytes().await {
        Ok(body) => body,
        Err(err) => {
            outcome.failed += 1;
            outcome.example(format!("{url}: {err}"));
            return;
        }
    };
    outcome.latencies.push(started.elapsed());
    outcome.bytes += body.len();
    if status.is_success() {
        outcome.ok += 1;
        // Masters PGNs are not JSON.
        if is_json && let Err(err) = validate(&body) {
            outcome.invalid += 1;
            outcome.example(format!("{url}: {err}"));
        }
        return;
    }
    if status.is_client_error() {
        outcome.client_errors += 1;
    } else if status.is_server_error() {
        outcome.server_errors += 1;
    } else {
        outcome.other_statuses += 1;
    }
    outcome.example(format!(
        "{url}: {status} {}",
        String::from_utf8_lossy(&body[..body.len().min(200)])
    ));
}

/// Sends the requests with the given concurrency. Returns the outcome for
/// each target.
async fn replay(
    client: &reqwest::Client,
    targets: Arc<[Target]>,
    requests: Arc<[Request]>,
    concurrency: usize,
) -> Vec<Outcome> {
    let next = Arc::new(AtomicUsize::new(0));
    let mut tasks = JoinSet::new();
    for _ in 0..concurrency.max(1) {
        let client = client.clone();
        let targets = Arc::clone(&targets);
        let requests = Arc::clone(&requests);
        let next = Arc::clone(&next);
        tasks.spawn(async move {
            let mut outcomes = vec![Outcome::default(); targets.len()];
            while let Some(req) = requests.get(next.fetch_add(1, Ordering::Relaxed)) {
                let url = format!("{}{}", targets[req.target].url, req.path);
                request(&client, &url, &mut outcomes[req.target]).await;
            }
            outcomes
        });
    }
    let mut outcomes = vec![Outcome::default(); targets.len()];
    while let Some(task) = tasks.join_next().await {
        for (outcome, other) in outcomes.iter_mut().zip(task.expect("replay task")) {
            outcome.merge(&other);
        }
    }
    outcomes
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let i = ((sorted.len() as f64 * p).ceil() as usize).clamp(1, sorted.len()) - 1;
    sorted[i]
}

fn report(name: &str, outcome: &mut Outcome, elapsed: Duration) {
    outcome.latencies.sort_unstable();
    let latencies = &outcome.latencies;
    let n = latencies.len() + outcome.failed;
    println!(
        "{name}: {n} requests in {elapsed:.1?}: {:.0} req/s, {:.1} MB",
        n as f64 / elapsed.as_secs_f64(),
        outcome.bytes as f64 / 1e6,
    );
    println!(
        "2xx {} (invalid {}), 4xx {}, 5xx {}, other {}, failed {}",
        outcome.ok,
        outcome.invalid,
        outcome.client_errors,
        outcome.server_errors,
        outcome.other_statuses,
        outcome.failed,
    );
    if !latencies.is_empty() {
        let mean = latencies.iter().sum::<Duration>() / latencies.len() as u32;
        println!(
            "latency mean {mean:.2?}, p50 {:.2?}, p90 {:.2?}, p99 {:.2?}, p99.9 {:.2?}, max {:.2?}",
            percentile(latencies, 0.5),
            percentile(latencies, 0.9),
            percentile(latencies, 0.99),
            percentile(latencies, 0.999),
            latencies.last().copied().unwrap_or_default(),
        );
    }
    for example in &outcome.examples {
        println!("example: {example}");
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let targets: Vec<Target> = [
        (Kind::Lichess, "lichess", &args.lichess_url),
        (Kind::Masters, "masters", &args.masters_url),
    ]
    .into_iter()
    .filter_map(|(kind, name, url)| {
        url.as_ref().map(|url| Target {
            kind,
            name,
            url: url.trim_end_matches('/').to_owned(),
        })
    })
    .collect();
    let (requests, skipped) = match read_requests(&args, &targets) {
        Ok(requests) => requests,
        Err(err) => {
            eprintln!("error: {}: {err}", args.log.display());
            std::process::exit(1);
        }
    };
    let warm_up = args.warm_up.min(requests.len());
    eprintln!(
        "{} requests from {} ({warm_up} warm-up, {skipped} lines skipped), concurrency {}",
        requests.len(),
        args.log.display(),
        args.concurrency,
    );
    for (i, target) in targets.iter().enumerate() {
        eprintln!(
            "  {} at {}: {} requests",
            target.name,
            target.url,
            requests.iter().filter(|req| req.target == i).count()
        );
    }
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(args.concurrency)
        .timeout(Duration::from_secs(args.timeout))
        .build()
        .expect("http client");
    let targets: Arc<[Target]> = targets.into();
    let mut requests = requests;
    let measured: Arc<[Request]> = requests.split_off(warm_up).into();

    if warm_up > 0 {
        let started = Instant::now();
        let outcomes = replay(
            &client,
            Arc::clone(&targets),
            requests.into(),
            args.concurrency,
        )
        .await;
        eprintln!(
            "warmed up with {warm_up} requests in {:.1?} ({} failed)",
            started.elapsed(),
            outcomes.iter().map(|outcome| outcome.failed).sum::<usize>()
        );
    }

    let started = Instant::now();
    let mut outcomes = replay(&client, Arc::clone(&targets), measured, args.concurrency).await;
    let elapsed = started.elapsed();
    for (target, outcome) in targets.iter().zip(&mut outcomes) {
        report(target.name, outcome, elapsed);
    }
    if targets.len() > 1 {
        let mut total = Outcome::default();
        for outcome in &outcomes {
            total.merge(outcome);
        }
        report("total", &mut total, elapsed);
    }
}
