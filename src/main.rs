//! Kringle: a tiny Kris Kringle / Secret Santa gift swap server.
//!
//! One static binary, small enough for a home router (built for the
//! GL.iNet Flint 2). A host starts a group, shares one invite link, adds
//! "keep apart" rules and draws names. No accounts: three kinds of
//! unguessable link (admin, invite, personal). State lives in one JSON file
//! that is rewritten atomically after every change.
//!
//! Everything is in this file: config, storage, the draw, HTTP handlers,
//! HTML (maud), CSS and the little JavaScript. Fonts and the two
//! illustrations are compiled in from `static/`.

use std::{
    fs,
    io::{self, Write as _},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Router,
    extract::{DefaultBodyLimit, Form, Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use maud::{DOCTYPE, Markup, PreEscaped, html};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

const NAME_MAX: usize = 40;
const GROUP_MAX: usize = 60;
const WISHES_MAX: usize = 600;
const BUDGET_MAX: usize = 6;
const MEMBERS_MAX: usize = 60;
const GROUPS_MAX: usize = 1000;
/// Wishes longer than this show a preview on the gift tag, with the rest
/// behind "Show all wishes".
const WISHES_PREVIEW: usize = 240;
/// Names longer than this show the first name big and the full name small.
const TAG_NAME_FULL: usize = 14;

// ---------------------------------------------------------------------------
// Config and entry point
// ---------------------------------------------------------------------------

struct Config {
    addr: String,
    data: PathBuf,
    public_url: Option<String>,
    keep_days: u64,
}

const USAGE: &str = "\
kringle: a tiny Kris Kringle / Secret Santa gift swap server

USAGE: kringle [--addr HOST:PORT] [--data FILE] [--public-url URL] [--keep-days N]

  --addr        where to listen            (env KRINGLE_ADDR, default 0.0.0.0:8787)
  --data        JSON file for all groups   (env KRINGLE_DATA, default ./kringle.json)
  --public-url  base for links people get, e.g. http://192.168.8.1:8787
                (env KRINGLE_PUBLIC_URL, default: the Host the browser used)
  --keep-days   delete groups this many days after they were created
                (env KRINGLE_KEEP_DAYS, default 120)
";

fn config() -> Config {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let mut cfg = Config {
        addr: env("KRINGLE_ADDR").unwrap_or_else(|| "0.0.0.0:8787".into()),
        data: env("KRINGLE_DATA")
            .unwrap_or_else(|| "kringle.json".into())
            .into(),
        public_url: env("KRINGLE_PUBLIC_URL"),
        keep_days: env("KRINGLE_KEEP_DAYS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(120),
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next().unwrap_or_else(|| {
                eprintln!("{name} needs a value\n\n{USAGE}");
                std::process::exit(2)
            })
        };
        match arg.as_str() {
            "--addr" => cfg.addr = value("--addr"),
            "--data" => cfg.data = value("--data").into(),
            "--public-url" => cfg.public_url = Some(value("--public-url")),
            "--keep-days" => {
                cfg.keep_days = value("--keep-days").parse().unwrap_or_else(|_| {
                    eprintln!("--keep-days needs a whole number");
                    std::process::exit(2)
                })
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0)
            }
            "-V" | "--version" => {
                println!("kringle {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0)
            }
            other => {
                eprintln!("unknown argument: {other}\n\n{USAGE}");
                std::process::exit(2)
            }
        }
    }
    cfg.public_url = cfg.public_url.map(|u| u.trim_end_matches('/').to_string());
    cfg
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cfg = config();
    let store = Store::load(cfg.data.clone()).unwrap_or_else(|e| {
        eprintln!("kringle: can't read {}: {e}", cfg.data.display());
        std::process::exit(1)
    });
    let listener = tokio::net::TcpListener::bind(&cfg.addr)
        .await
        .unwrap_or_else(|e| {
            eprintln!("kringle: can't listen on {}: {e}", cfg.addr);
            std::process::exit(1)
        });
    eprintln!(
        "kringle {} listening on http://{} (data: {})",
        env!("CARGO_PKG_VERSION"),
        cfg.addr,
        cfg.data.display()
    );
    let app = Arc::new(App {
        store: Mutex::new(store),
        public_url: cfg.public_url,
        keep_secs: cfg.keep_days * 86_400,
    });
    if let Err(e) = axum::serve(listener, router(app))
        .with_graceful_shutdown(shutdown())
        .await
    {
        eprintln!("kringle: server error: {e}");
        std::process::exit(1)
    }
}

async fn shutdown() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Default)]
struct Data {
    groups: Vec<Group>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Group {
    name: String,
    host: String,
    budget: String,
    admin: String,
    invite: String,
    created: u64,
    drawn: Option<u64>,
    next_id: u32,
    members: Vec<Member>,
    rules: Vec<Rule>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Member {
    id: u32,
    name: String,
    wishes: String,
    token: String,
    host: bool,
    opened: bool,
    gives_to: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Rule {
    giver: u32,
    receiver: u32,
    both: bool,
}

struct Store {
    path: PathBuf,
    data: Data,
}

impl Store {
    fn load(path: PathBuf) -> io::Result<Store> {
        let data = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Data::default(),
            Err(e) => return Err(e),
        };
        Ok(Store { path, data })
    }

    /// Write to a temp file, fsync, then rename over the old file, so a
    /// power cut never leaves a half-written file.
    fn save(&self) -> io::Result<()> {
        let bytes = serde_json::to_vec(&self.data).map_err(io::Error::other)?;
        let mut tmp = self.path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &self.path)
    }

    fn by_admin(&mut self, t: &str) -> Option<&mut Group> {
        self.data.groups.iter_mut().find(|g| g.admin == t)
    }

    fn by_invite(&mut self, t: &str) -> Option<&mut Group> {
        self.data.groups.iter_mut().find(|g| g.invite == t)
    }

    fn by_personal(&mut self, t: &str) -> Option<(&mut Group, usize)> {
        self.data.groups.iter_mut().find_map(|g| {
            let i = g.members.iter().position(|m| m.token == t)?;
            Some((g, i))
        })
    }

    fn prune(&mut self, now: u64, keep_secs: u64) {
        self.data
            .groups
            .retain(|g| now.saturating_sub(g.created) < keep_secs);
    }
}

impl Group {
    fn index_of(&self, id: u32) -> Option<usize> {
        self.members.iter().position(|m| m.id == id)
    }

    fn member(&self, id: u32) -> Option<&Member> {
        self.members.iter().find(|m| m.id == id)
    }

    fn allowed(&self) -> draw::Allowed {
        let mut bans = Vec::new();
        for r in &self.rules {
            if let (Some(g), Some(v)) = (self.index_of(r.giver), self.index_of(r.receiver)) {
                bans.push((g, v));
                if r.both {
                    bans.push((v, g));
                }
            }
        }
        draw::allowed(self.members.len(), &bans)
    }

    fn verdict(&self) -> draw::Verdict {
        draw::verdict(&self.allowed(), self.members.iter().position(|m| m.host))
    }

    fn host_member(&self) -> Option<&Member> {
        self.members.iter().find(|m| m.host)
    }
}

// ---------------------------------------------------------------------------
// The draw
// ---------------------------------------------------------------------------

/// Who may give to whom, whether a fair and secret draw exists, and the draw
/// itself. People are indexed `0..n`; `allowed[g][r]` means `g` may give to
/// `r`. A draw is a perfect matching in that bipartite graph.
mod draw {
    pub type Allowed = Vec<Vec<bool>>;

    /// Everyone may give to everyone but themselves, minus the banned pairs.
    pub fn allowed(n: usize, bans: &[(usize, usize)]) -> Allowed {
        let mut a = vec![vec![true; n]; n];
        for (i, row) in a.iter_mut().enumerate() {
            row[i] = false;
        }
        for &(g, r) in bans {
            a[g][r] = false;
        }
        a
    }

    /// One perfect matching giver -> receiver, if any exists (Kuhn's algorithm).
    fn matching(a: &Allowed) -> Option<Vec<usize>> {
        let n = a.len();
        let mut owner: Vec<Option<usize>> = vec![None; n];
        for g in 0..n {
            let mut seen = vec![false; n];
            if !augment(a, g, &mut seen, &mut owner) {
                return None;
            }
        }
        let mut out = vec![0; n];
        for (r, g) in owner.iter().enumerate() {
            out[(*g)?] = r;
        }
        Some(out)
    }

    fn augment(a: &Allowed, g: usize, seen: &mut [bool], owner: &mut [Option<usize>]) -> bool {
        for r in 0..a.len() {
            if a[g][r] && !seen[r] {
                seen[r] = true;
                let free = match owner[r] {
                    None => true,
                    Some(h) => augment(a, h, seen, owner),
                };
                if free {
                    owner[r] = Some(g);
                    return true;
                }
            }
        }
        false
    }

    /// The pairs that appear in at least one valid draw, or `None` if there
    /// is no valid draw at all.
    ///
    /// Given one perfect matching M, orient unmatched edges giver -> receiver
    /// and matched edges receiver -> giver. An unmatched edge lies in some
    /// perfect matching exactly when both ends share a strongly connected
    /// component.
    pub fn feasible(a: &Allowed) -> Option<Allowed> {
        let n = a.len();
        let m = matching(a)?;
        let mut adj = vec![Vec::new(); 2 * n];
        for g in 0..n {
            for r in 0..n {
                if a[g][r] {
                    if m[g] == r {
                        adj[n + r].push(g);
                    } else {
                        adj[g].push(n + r);
                    }
                }
            }
        }
        let comp = components(&adj);
        Some(
            (0..n)
                .map(|g| {
                    (0..n)
                        .map(|r| a[g][r] && (m[g] == r || comp[g] == comp[n + r]))
                        .collect()
                })
                .collect(),
        )
    }

    /// Strongly connected components (Kosaraju). Graphs here have at most a
    /// few hundred nodes, so plain recursion is fine.
    fn components(adj: &[Vec<usize>]) -> Vec<usize> {
        fn order(v: usize, adj: &[Vec<usize>], seen: &mut [bool], out: &mut Vec<usize>) {
            seen[v] = true;
            for &w in &adj[v] {
                if !seen[w] {
                    order(w, adj, seen, out);
                }
            }
            out.push(v);
        }
        fn mark(v: usize, radj: &[Vec<usize>], comp: &mut [usize], c: usize) {
            comp[v] = c;
            for &w in &radj[v] {
                if comp[w] == usize::MAX {
                    mark(w, radj, comp, c);
                }
            }
        }
        let n = adj.len();
        let mut seen = vec![false; n];
        let mut finish = Vec::with_capacity(n);
        for v in 0..n {
            if !seen[v] {
                order(v, adj, &mut seen, &mut finish);
            }
        }
        let mut radj = vec![Vec::new(); n];
        for (v, ws) in adj.iter().enumerate() {
            for &w in ws {
                radj[w].push(v);
            }
        }
        let mut comp = vec![usize::MAX; n];
        let mut c = 0;
        for &v in finish.iter().rev() {
            if comp[v] == usize::MAX {
                mark(v, &radj, &mut comp, c);
                c += 1;
            }
        }
        comp
    }

    /// Pin `g -> r`: `g` gives to nobody else and nobody else gives to `r`.
    fn fix(a: &Allowed, g: usize, r: usize) -> Allowed {
        let mut b = a.clone();
        for (x, cell) in b[g].iter_mut().enumerate() {
            *cell = x == r;
        }
        for (y, row) in b.iter_mut().enumerate() {
            if y != g {
                row[r] = false;
            }
        }
        b
    }

    fn count(row: &[bool]) -> usize {
        row.iter().filter(|&&x| x).count()
    }

    #[derive(Debug, PartialEq, Eq)]
    pub enum Verdict {
        /// Fewer than three people.
        TooFew,
        /// This person has nobody left they are allowed to give to.
        Stuck(usize),
        /// Every possible draw breaks a rule.
        Impossible,
        /// The rules leave this person exactly one option, so the host would
        /// know who they got.
        Forced(usize),
        /// A fair, secret draw exists. `host_can_deduce`: the host is in the
        /// draw and, once they see their own tag, could work out someone
        /// else's.
        Ready { host_can_deduce: bool },
    }

    pub fn verdict(a: &Allowed, host: Option<usize>) -> Verdict {
        let n = a.len();
        if n < 3 {
            return Verdict::TooFew;
        }
        if let Some(g) = (0..n).find(|&g| count(&a[g]) == 0) {
            return Verdict::Stuck(g);
        }
        let Some(f) = feasible(a) else {
            return Verdict::Impossible;
        };
        if let Some(g) = (0..n).find(|&g| count(&f[g]) == 1) {
            return Verdict::Forced(g);
        }
        let host_can_deduce = host.is_some_and(|h| {
            (0..n)
                .filter(|&r| f[h][r])
                .any(|r| match feasible(&fix(a, h, r)) {
                    Some(ff) => (0..n).any(|g| g != h && count(&ff[g]) == 1),
                    None => false,
                })
        });
        Verdict::Ready { host_can_deduce }
    }

    /// A random valid draw: givers in random order, each picking uniformly
    /// among the receivers that still leave a valid draw for everyone else.
    /// `rand(k)` returns a number in `0..k`.
    pub fn draw(a: &Allowed, mut rand: impl FnMut(usize) -> usize) -> Option<Vec<usize>> {
        let n = a.len();
        let mut order: Vec<usize> = (0..n).collect();
        for i in (1..n).rev() {
            order.swap(i, rand(i + 1));
        }
        let mut cur = a.clone();
        for &g in &order {
            let f = feasible(&cur)?;
            let opts: Vec<usize> = (0..n).filter(|&r| f[g][r]).collect();
            let r = opts[rand(opts.len())];
            cur = fix(&cur, g, r);
        }
        matching(&cur)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn lcg(seed: u64) -> impl FnMut(usize) -> usize {
            let mut s = seed;
            move |k| {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((s >> 33) as usize) % k
            }
        }

        #[test]
        fn too_few_and_stuck() {
            assert_eq!(verdict(&allowed(2, &[]), None), Verdict::TooFew);
            assert_eq!(
                verdict(&allowed(3, &[(0, 1), (0, 2)]), None),
                Verdict::Stuck(0)
            );
        }

        #[test]
        fn partners_in_three_is_impossible() {
            assert_eq!(
                verdict(&allowed(3, &[(1, 2), (2, 1)]), None),
                Verdict::Impossible
            );
        }

        #[test]
        fn forced_pair_is_caught() {
            let a = allowed(4, &[(0, 2), (0, 3)]);
            assert_eq!(verdict(&a, None), Verdict::Forced(0));
        }

        #[test]
        fn host_in_group_of_three_can_deduce() {
            let ready = |d| Verdict::Ready { host_can_deduce: d };
            assert_eq!(verdict(&allowed(3, &[]), Some(0)), ready(true));
            assert_eq!(verdict(&allowed(3, &[]), None), ready(false));
            assert_eq!(verdict(&allowed(4, &[]), Some(0)), ready(false));
        }

        #[test]
        fn feasible_matches_brute_force() {
            fn perms(k: usize, p: &mut Vec<usize>, a: &Allowed, seen: &mut Allowed) {
                if k == p.len() {
                    if (0..p.len()).all(|g| a[g][p[g]]) {
                        for g in 0..p.len() {
                            seen[g][p[g]] = true;
                        }
                    }
                    return;
                }
                for i in k..p.len() {
                    p.swap(k, i);
                    perms(k + 1, p, a, seen);
                    p.swap(k, i);
                }
            }
            let a = allowed(5, &[(0, 1), (1, 0), (2, 3), (4, 0), (3, 4)]);
            let mut seen = vec![vec![false; 5]; 5];
            perms(0, &mut (0..5).collect(), &a, &mut seen);
            assert_eq!(feasible(&a).unwrap(), seen);
        }

        #[test]
        fn draw_respects_rules() {
            let a = allowed(7, &[(1, 2), (2, 1), (3, 4), (4, 3), (6, 0)]);
            for seed in 0..200 {
                let d = draw(&a, lcg(seed)).expect("draw");
                let mut got = [false; 7];
                for (g, &r) in d.iter().enumerate() {
                    assert!(a[g][r], "rule broken");
                    assert!(!got[r], "someone got two gifts");
                    got[r] = true;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("system randomness");
    b
}

fn rand_below(k: usize) -> usize {
    (u64::from_le_bytes(random_bytes::<8>()) % k as u64) as usize
}

/// 128 random bits as 26 lowercase base32 characters.
fn token() -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let (mut out, mut acc, mut bits) = (String::with_capacity(26), 0u32, 0);
    for byte in random_bytes::<16>() {
        acc = (acc << 8) | byte as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((acc >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// A friendly invite link: `snowy-otter-` plus 80 random bits.
fn invite_token() -> String {
    const ADJ: [&str; 12] = [
        "snowy", "merry", "jolly", "cosy", "starry", "frosty", "gentle", "bright", "golden",
        "quiet", "happy", "twinkly",
    ];
    const NOUN: [&str; 12] = [
        "otter", "robin", "fox", "lamb", "owl", "wren", "deer", "dove", "star", "bell", "sheep",
        "shepherd",
    ];
    let [a, b] = random_bytes::<2>();
    format!(
        "{}-{}-{}",
        ADJ[a as usize % ADJ.len()],
        NOUN[b as usize % NOUN.len()],
        &token()[..16]
    )
}

/// One line of text: collapse whitespace, drop control characters, cap length.
fn clean_line(s: &str, max: usize) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .take(max)
        .collect()
}

/// Free text: keep line breaks, drop other control characters, cap length.
fn clean_text(s: &str, max: usize) -> String {
    let s = s.replace("\r\n", "\n").replace('\r', "\n");
    let kept: String = s
        .chars()
        .filter(|&c| c == '\n' || !c.is_control())
        .collect();
    let lines: Vec<&str> = kept.lines().map(str::trim_end).collect();
    lines.join("\n").trim().chars().take(max).collect()
}

fn first_word(name: &str) -> &str {
    name.split_whitespace().next().unwrap_or(name)
}

/// First and last word, skipping suffixes: "Bartholomew J. Winterbottom III" -> "BW".
fn initials(name: &str) -> String {
    let words: Vec<&str> = name
        .split_whitespace()
        .enumerate()
        .filter(|(i, w)| {
            let bare = w.trim_end_matches('.').to_ascii_lowercase();
            *i == 0 || !matches!(bare.as_str(), "jr" | "sr" | "i" | "ii" | "iii" | "iv")
        })
        .map(|(_, w)| w)
        .collect();
    let mut out = String::new();
    for w in [
        words.first(),
        if words.len() > 1 { words.last() } else { None },
    ]
    .into_iter()
    .flatten()
    {
        out.extend(
            w.chars()
                .next()
                .map(|c| c.to_uppercase().collect::<String>()),
        );
    }
    out
}

/// "1 Dec" for a UNIX time (UTC).
fn day_month(ts: u64) -> String {
    // Howard Hinnant's civil_from_days.
    let z = (ts / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!("{d} {}", MONTHS[(m - 1) as usize])
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn qr_svg(text: &str) -> String {
    use qrcodegen::{QrCode, QrCodeEcc};
    let Ok(qr) = QrCode::encode_text(text, QrCodeEcc::Medium) else {
        return String::new();
    };
    let size = qr.size();
    let mut path = String::new();
    for y in 0..size {
        for x in 0..size {
            if qr.get_module(x, y) {
                path.push_str(&format!("M{},{}h1v1h-1z", x + 4, y + 4));
            }
        }
    }
    let full = size + 8;
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {full} {full}" width="200" height="200" shape-rendering="crispEdges" role="img" aria-label="QR code for the invite link"><rect width="{full}" height="{full}" fill="#fff"/><path d="{path}" fill="#1A0F05"/></svg>"##
    )
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

// ---------------------------------------------------------------------------
// HTTP plumbing
// ---------------------------------------------------------------------------

struct App {
    store: Mutex<Store>,
    public_url: Option<String>,
    keep_secs: u64,
}

type Shared = Arc<App>;

fn lock(app: &App) -> MutexGuard<'_, Store> {
    app.store.lock().unwrap_or_else(|e| e.into_inner())
}

fn router(app: Shared) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/groups", post(create))
        .route("/g/{t}", get(admin))
        .route("/g/{t}/rules", post(add_rule))
        .route("/g/{t}/rules/{i}/delete", post(remove_rule))
        .route("/g/{t}/people/{id}/delete", post(remove_person))
        .route("/g/{t}/draw", post(draw_names))
        .route("/g/{t}/undraw", post(undraw))
        .route("/j/{t}", get(join).post(join_post))
        .route("/p/{t}", get(me))
        .route("/p/{t}/wishes", post(save_wishes))
        .route("/p/{t}/open", post(unwrap_tag))
        .route("/p/{t}/state", get(draw_state))
        .route("/static/{file}", get(asset))
        .fallback(fallback)
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(middleware::map_response(security_headers))
        .with_state(app)
}

async fn security_headers(mut res: Response) -> Response {
    let h = res.headers_mut();
    let mut set = |k: header::HeaderName, v: &'static str| {
        h.entry(k).or_insert(HeaderValue::from_static(v));
    };
    set(
        header::CONTENT_SECURITY_POLICY,
        "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; script-src 'self'; \
         form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
    );
    // Admin and personal links carry their secret in the URL: never leak it.
    set(header::REFERRER_POLICY, "no-referrer");
    set(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    set(header::X_FRAME_OPTIONS, "DENY");
    set(header::CACHE_CONTROL, "no-store");
    res
}

/// Base for links we hand out: the configured public URL, else whatever
/// host the browser used to reach us.
fn base_url(app: &App, headers: &HeaderMap) -> String {
    if let Some(u) = &app.public_url {
        return u.clone();
    }
    let get = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).map(str::trim);
    let host = get("x-forwarded-host")
        .or_else(|| get("host"))
        .filter(|h| {
            !h.is_empty()
                && h.chars()
                    .all(|c| c.is_ascii_alphanumeric() || ".-:[]".contains(c))
        })
        .unwrap_or("localhost");
    let proto = match get("x-forwarded-proto") {
        Some("https") => "https",
        _ => "http",
    };
    format!("{proto}://{host}")
}

fn page(markup: Markup) -> Response {
    Html(markup.into_string()).into_response()
}

fn to(path: String) -> Response {
    Redirect::to(&path).into_response()
}

fn not_found() -> Response {
    let body = layout(
        "Link not found · Kringle",
        html! {},
        html! {
            section.panel {
                h1 { "This link doesn’t work" }
                p { "It may have a typo, or the group may have been deleted. Ask whoever sent it to check." }
                a.btn href="/" { "Start a new gift swap" }
            }
        },
    );
    (StatusCode::NOT_FOUND, Html(body.into_string())).into_response()
}

fn save_failed(e: io::Error) -> Response {
    eprintln!("kringle: saving failed: {e}");
    let body = layout(
        "Couldn’t save · Kringle",
        html! {},
        html! {
            section.panel {
                h1 { "Couldn’t save that" }
                p { "The server couldn’t write its data file. Nothing was changed. Try again in a moment." }
            }
        },
    );
    (StatusCode::INTERNAL_SERVER_ERROR, Html(body.into_string())).into_response()
}

async fn fallback() -> Response {
    not_found()
}

async fn asset(Path(file): Path<String>) -> Response {
    let (kind, body): (&str, &'static [u8]) = match file.as_str() {
        "style.css" => ("text/css; charset=utf-8", CSS.as_bytes()),
        "app.js" => ("text/javascript; charset=utf-8", JS.as_bytes()),
        "town.svg" => ("image/svg+xml", include_bytes!("../static/town.svg")),
        "clouds.svg" => ("image/svg+xml", include_bytes!("../static/clouds.svg")),
        "fell.woff2" => ("font/woff2", include_bytes!("../static/fonts/fell.woff2")),
        "fell-italic.woff2" => (
            "font/woff2",
            include_bytes!("../static/fonts/fell-italic.woff2"),
        ),
        _ => return not_found(),
    };
    (
        [
            (header::CONTENT_TYPE, kind),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        body,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Handlers: start a group
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(default)]
struct CreateForm {
    group: String,
    host: String,
    budget: String,
    plays: Option<String>,
}

async fn index() -> Response {
    page(create_page(
        &CreateForm {
            plays: Some("1".into()),
            ..Default::default()
        },
        None,
    ))
}

async fn create(State(app): State<Shared>, Form(f): Form<CreateForm>) -> Response {
    let name = clean_line(&f.group, GROUP_MAX);
    let host = clean_line(&f.host, NAME_MAX);
    let budget: String = f
        .budget
        .chars()
        .filter(char::is_ascii_digit)
        .take(BUDGET_MAX)
        .collect();
    let error = if name.is_empty() {
        Some("Give the group a name.")
    } else if host.is_empty() {
        Some("Add your name so people know who invited them.")
    } else {
        None
    };
    if let Some(e) = error {
        return page(create_page(&f, Some(e)));
    }
    let mut st = lock(&app);
    st.prune(now(), app.keep_secs);
    if st.data.groups.len() >= GROUPS_MAX {
        return page(create_page(
            &f,
            Some("This Kringle server is full. Ask whoever runs it to make room."),
        ));
    }
    let mut g = Group {
        name,
        host: host.clone(),
        budget,
        admin: token(),
        invite: invite_token(),
        created: now(),
        drawn: None,
        next_id: 1,
        members: Vec::new(),
        rules: Vec::new(),
    };
    if f.plays.is_some() {
        g.members.push(Member {
            id: 1,
            name: host,
            wishes: String::new(),
            token: token(),
            host: true,
            opened: false,
            gives_to: None,
        });
        g.next_id = 2;
    }
    let admin = g.admin.clone();
    st.data.groups.push(g);
    if let Err(e) = st.save() {
        st.data.groups.pop();
        return save_failed(e);
    }
    to(format!("/g/{admin}"))
}

// ---------------------------------------------------------------------------
// Handlers: host dashboard
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(default)]
struct AdminQuery {
    err: Option<String>,
}

async fn admin(
    State(app): State<Shared>,
    Path(t): Path<String>,
    Query(q): Query<AdminQuery>,
    headers: HeaderMap,
) -> Response {
    let base = base_url(&app, &headers);
    let mut st = lock(&app);
    let Some(g) = st.by_admin(&t) else {
        return not_found();
    };
    let error = match q.err.as_deref() {
        Some("same") => Some("Pick two different people."),
        Some("dup") => Some("That rule already exists."),
        Some("draw") => Some("Names couldn’t be drawn. Check the rules below."),
        Some("drawn") => Some("Names are already drawn, so that can’t change now."),
        _ => None,
    };
    page(admin_page(g, &base, error))
}

/// Run `f` on the group behind an admin link, save, and redirect back to the
/// dashboard (`f` returns an error code for the dashboard to explain, or
/// `None`). `anchor` is where on the page to land.
fn admin_change(
    app: &App,
    t: &str,
    anchor: &str,
    f: impl FnOnce(&mut Group) -> Option<&'static str>,
) -> Response {
    let mut st = lock(app);
    let Some(g) = st.by_admin(t) else {
        return not_found();
    };
    let before = g.clone();
    if let Some(code) = f(g) {
        return to(format!("/g/{t}?err={code}#{anchor}"));
    }
    if let Err(e) = st.save() {
        if let Some(g) = st.by_admin(t) {
            *g = before;
        }
        return save_failed(e);
    }
    to(format!("/g/{t}#{anchor}"))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RuleForm {
    giver: String,
    receiver: String,
    both: Option<String>,
}

async fn add_rule(
    State(app): State<Shared>,
    Path(t): Path<String>,
    Form(f): Form<RuleForm>,
) -> Response {
    admin_change(&app, &t, "rules", |g| {
        if g.drawn.is_some() {
            return Some("drawn");
        }
        let (Ok(giver), Ok(receiver)) = (f.giver.parse::<u32>(), f.receiver.parse::<u32>()) else {
            return Some("same");
        };
        if giver == receiver || g.index_of(giver).is_none() || g.index_of(receiver).is_none() {
            return Some("same");
        }
        let both = f.both.is_some();
        let dup = g.rules.iter().any(|r| {
            (r.giver == giver && r.receiver == receiver)
                || (r.both && r.giver == receiver && r.receiver == giver)
                || (both && r.giver == receiver && r.receiver == giver)
        });
        if dup {
            return Some("dup");
        }
        g.rules.push(Rule {
            giver,
            receiver,
            both,
        });
        None
    })
}

async fn remove_rule(State(app): State<Shared>, Path((t, i)): Path<(String, usize)>) -> Response {
    admin_change(&app, &t, "rules", |g| {
        if g.drawn.is_some() {
            return Some("drawn");
        }
        if i < g.rules.len() {
            g.rules.remove(i);
        }
        None
    })
}

async fn remove_person(State(app): State<Shared>, Path((t, id)): Path<(String, u32)>) -> Response {
    admin_change(&app, &t, "people", |g| {
        if g.drawn.is_some() {
            return Some("drawn");
        }
        g.members.retain(|m| m.id != id || m.host);
        g.rules.retain(|r| {
            g.members.iter().any(|m| m.id == r.giver)
                && g.members.iter().any(|m| m.id == r.receiver)
        });
        None
    })
}

async fn draw_names(State(app): State<Shared>, Path(t): Path<String>) -> Response {
    admin_change(&app, &t, "draw", |g| {
        if g.drawn.is_some() {
            return Some("drawn");
        }
        if !matches!(g.verdict(), draw::Verdict::Ready { .. }) {
            return Some("draw");
        }
        let Some(assignment) = draw::draw(&g.allowed(), rand_below) else {
            return Some("draw");
        };
        let ids: Vec<u32> = g.members.iter().map(|m| m.id).collect();
        for (m, r) in g.members.iter_mut().zip(assignment) {
            m.gives_to = Some(ids[r]);
            m.opened = false;
        }
        g.drawn = Some(now());
        None
    })
}

async fn undraw(State(app): State<Shared>, Path(t): Path<String>) -> Response {
    admin_change(&app, &t, "top", |g| {
        g.drawn = None;
        for m in &mut g.members {
            m.gives_to = None;
            m.opened = false;
        }
        None
    })
}

// ---------------------------------------------------------------------------
// Handlers: joining
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(default)]
struct JoinForm {
    name: String,
    wishes: String,
}

async fn join(State(app): State<Shared>, Path(t): Path<String>, headers: HeaderMap) -> Response {
    let mut st = lock(&app);
    let Some(g) = st.by_invite(&t) else {
        return not_found();
    };
    let returning =
        cookie(&headers, "kringle").and_then(|c| g.members.iter().find(|m| m.token == c));
    page(join_page(g, returning, &JoinForm::default(), None))
}

async fn join_post(
    State(app): State<Shared>,
    Path(t): Path<String>,
    Form(f): Form<JoinForm>,
) -> Response {
    let mut st = lock(&app);
    let Some(g) = st.by_invite(&t) else {
        return not_found();
    };
    let name = clean_line(&f.name, NAME_MAX);
    let wishes = clean_text(&f.wishes, WISHES_MAX);
    let error = if g.drawn.is_some() {
        Some("Names have already been drawn, so this group is closed.".to_string())
    } else if name.is_empty() {
        Some("Add your name so the host knows who you are.".to_string())
    } else if g
        .members
        .iter()
        .any(|m| m.name.to_lowercase() == name.to_lowercase())
    {
        Some(format!(
            "Someone called {name} is already in. Add a surname or a nickname so people can tell you apart."
        ))
    } else if g.members.len() >= MEMBERS_MAX {
        Some("This group is full.".to_string())
    } else {
        None
    };
    if let Some(e) = error {
        return page(join_page(g, None, &f, Some(&e)));
    }
    let token = token();
    g.members.push(Member {
        id: g.next_id,
        name,
        wishes,
        token: token.clone(),
        host: false,
        opened: false,
        gives_to: None,
    });
    g.next_id += 1;
    if let Err(e) = st.save() {
        if let Some(g) = st.by_invite(&t) {
            g.members.pop();
        }
        return save_failed(e);
    }
    // Lets them get back in by reopening the invite link on the same phone.
    let set_cookie =
        format!("kringle={token}; Path=/j/{t}; Max-Age=15552000; HttpOnly; SameSite=Lax");
    (
        [(header::SET_COOKIE, set_cookie)],
        Redirect::to(&format!("/p/{token}")),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Handlers: personal page and gift tag
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(default)]
struct MeQuery {
    show: Option<String>,
}

async fn me(
    State(app): State<Shared>,
    Path(t): Path<String>,
    Query(q): Query<MeQuery>,
    headers: HeaderMap,
) -> Response {
    let base = base_url(&app, &headers);
    let mut st = lock(&app);
    let Some((g, i)) = st.by_personal(&t) else {
        return not_found();
    };
    let m = &g.members[i];
    if g.drawn.is_some()
        && let Some(to) = m.gives_to.and_then(|id| g.member(id))
    {
        return page(tag_page(g, m, to, q.show.is_some() && m.opened));
    }
    page(waiting_page(g, m, &base))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct WishForm {
    wishes: String,
    back: String,
}

async fn save_wishes(
    State(app): State<Shared>,
    Path(t): Path<String>,
    Form(f): Form<WishForm>,
) -> Response {
    let mut st = lock(&app);
    let Some((g, i)) = st.by_personal(&t) else {
        return not_found();
    };
    let before = std::mem::replace(&mut g.members[i].wishes, clean_text(&f.wishes, WISHES_MAX));
    if let Err(e) = st.save() {
        if let Some((g, i)) = st.by_personal(&t) {
            g.members[i].wishes = before;
        }
        return save_failed(e);
    }
    to(if f.back == "tag" {
        format!("/p/{t}?show=1#tag")
    } else {
        format!("/p/{t}#wishes")
    })
}

async fn unwrap_tag(State(app): State<Shared>, Path(t): Path<String>) -> Response {
    let mut st = lock(&app);
    let Some((g, i)) = st.by_personal(&t) else {
        return not_found();
    };
    if g.drawn.is_none() {
        return to(format!("/p/{t}"));
    }
    if !g.members[i].opened {
        g.members[i].opened = true;
        if let Err(e) = st.save() {
            return save_failed(e);
        }
    }
    to(format!("/p/{t}?show=1#tag"))
}

/// Polled by the waiting page: 200 once names are drawn, else 204.
async fn draw_state(State(app): State<Shared>, Path(t): Path<String>) -> Response {
    let mut st = lock(&app);
    match st.by_personal(&t) {
        Some((g, _)) if g.drawn.is_some() => StatusCode::OK.into_response(),
        Some(_) => StatusCode::NO_CONTENT.into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

fn layout(title: &str, header_right: Markup, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta name="robots" content="noindex";
                title { (title) }
                link rel="stylesheet" href="/static/style.css";
                script src="/static/app.js" defer {}
            }
            body {
                div.sky aria-hidden="true" {
                    div.sun {}
                    img.clouds src="/static/clouds.svg" alt="";
                }
                header.site {
                    a.brand href="/" { (PreEscaped(LOGO)) span { "Kringle" } }
                    (header_right)
                }
                div.band {}
                main #top { (body) }
                img.town src="/static/town.svg" alt="";
            }
        }
    }
}

fn icon(svg: &'static str) -> Markup {
    PreEscaped(svg.to_string())
}

fn create_page(f: &CreateForm, error: Option<&str>) -> Markup {
    layout(
        "Start a gift swap · Kringle",
        html! { span.tagline { "No sign-up. No app. Just a link." } },
        html! {
            form.panel.stack method="post" action="/groups" {
                div.row {
                    h1 { "Start a gift swap" }
                    span.aside { "takes 1 min" }
                }
                @if let Some(e) = error { p.error role="alert" { (e) } }
                label.field {
                    "Group name"
                    input type="text" name="group" maxlength=(GROUP_MAX) required placeholder="The Morgans’ Christmas" value=(f.group);
                }
                label.field {
                    "Your name"
                    input type="text" name="host" maxlength=(NAME_MAX) required placeholder="Rosa" autocomplete="given-name" value=(f.host);
                    span.hint { "Shown to everyone as the host." }
                }
                label.field {
                    "Budget"
                    span.money {
                        span aria-hidden="true" { "$" }
                        input type="text" name="budget" inputmode="numeric" maxlength=(BUDGET_MAX) placeholder="30" value=(f.budget);
                    }
                    span.hint { "Optional." }
                }
                label.check {
                    input type="checkbox" name="plays" value="1" checked[f.plays.is_some()];
                    "I’m in the draw too"
                }
                button.btn.primary.big type="submit" { (icon(GIFT_ICON)) "Create group" }
                p.lock { (icon(LOCK)) "No accounts. You’ll get a private admin link. It’s the only key to your group, so keep it safe." }
            }
        },
    )
}

fn admin_page(g: &Group, base: &str, error: Option<&str>) -> Markup {
    let admin_path = format!("/g/{}", g.admin);
    let admin_url = format!("{base}{admin_path}");
    let invite_url = format!("{base}/j/{}", g.invite);
    let name_of = |id: u32| g.member(id).map(|m| m.name.as_str()).unwrap_or("?");
    let verdict = g.verdict();
    let n = g.members.len();
    let (ready, problem): (bool, Option<(&str, String)>) = match &verdict {
        draw::Verdict::Ready { .. } => (true, None),
        draw::Verdict::TooFew => (
            false,
            Some((
                "Not enough people yet",
                "You need at least 3 people to draw names.".into(),
            )),
        ),
        draw::Verdict::Stuck(i) => (
            false,
            Some((
                "No fair draw is possible",
                format!(
                    "{} has no one left to buy for. Remove one of their rules.",
                    g.members[*i].name
                ),
            )),
        ),
        draw::Verdict::Impossible => (
            false,
            Some((
                "No fair draw is possible",
                "Every possible draw breaks a rule. Invite more people or remove a rule.".into(),
            )),
        ),
        draw::Verdict::Forced(i) => (
            false,
            Some((
                "This draw wouldn’t be secret",
                format!(
                    "These rules leave {} only one person to buy for, so you’d know who they got. Remove or loosen a rule.",
                    g.members[*i].name
                ),
            )),
        ),
    };
    let rule_problem = matches!(
        verdict,
        draw::Verdict::Stuck(_) | draw::Verdict::Impossible | draw::Verdict::Forced(_)
    );
    let host_can_deduce = matches!(
        verdict,
        draw::Verdict::Ready {
            host_can_deduce: true
        }
    );
    let rules_open = rule_problem || matches!(error, Some(e) if !e.starts_with("Names are"));
    let rules_label = if g.rules.is_empty() {
        "Keep some people apart (optional)".to_string()
    } else {
        format!(
            "Keep some people apart · {}",
            plural(g.rules.len(), "rule", "rules")
        )
    };
    let opened = g.members.iter().filter(|m| m.opened).count();
    let my_page = g.host_member().map(|m| format!("/p/{}", m.token));

    layout(
        &format!("{} · Host · Kringle", g.name),
        html! { span.badge { (icon(LOCK)) "Host view" } },
        html! {
            div.intro {
                span.plate { "Hosted by " (g.host) }
                h1.onsky { (g.name) }
                @if !g.budget.is_empty() { p.onsky-muted { "$" (g.budget) " budget" } }
            }
            div.adminlink {
                (icon(LOCK))
                p { strong { "This page is your admin link." } " Bookmark it and keep it private." }
                button.btn.small type="button" data-copy=(admin_url) { "Copy admin link" }
            }
            @if let Some(url) = &my_page {
                p.plate.small { "You’re in the draw too. " a href=(url) { "Your own page" } " has your wishes and, later, your gift tag." }
            }
            @if let Some(e) = error { p.error role="alert" { (e) } }

            @if let Some(at) = g.drawn {
                section.drawn-card #draw {
                    span.kicker { "Drawn " (day_month(at)) }
                    h2 { "Gift tags are out" }
                    p { strong { (opened) " of " (n) " have opened their tag" } }
                    div.bar role="img" aria-label=(format!("{opened} of {n} opened")) {
                        span style=(format!("width: {}%", if n == 0 { 0 } else { opened * 100 / n })) {}
                    }
                    p { "Everyone sees only their own tag. You see who’s opened theirs, never who they got." }
                }
                section.panel {
                    details.disclose {
                        summary { "See who’s opened their tag" }
                        ul.rows {
                            @for m in &g.members {
                                li {
                                    span.name title=(m.name) { (m.name) @if m.host { " (you)" } }
                                    @if m.opened { span.pill.yes { "Opened" } } @else { span.pill { "Not yet" } }
                                }
                            }
                        }
                    }
                    details.disclose {
                        summary { "More options" }
                        div.inset {
                            p { strong { "Someone lost their link?" } " They reopen the invite link on the same phone to get back in." }
                            p { strong { "Need to change the group?" } " Cancelling reopens joining. When you draw again, everyone gets a new tag." }
                            form method="post" action=(format!("{admin_path}/undraw")) data-confirm="Cancel the draw and reopen joining? Everyone’s gift tag disappears until you draw again." {
                                button.btn type="submit" { "Cancel draw & reopen" }
                            }
                        }
                    }
                }
            } @else {
                section.panel.step #invite {
                    div.stephead { span.num aria-hidden="true" { "1" } h2 { "Invite people" } }
                    p.muted { "Send this link to the group chat." }
                    div.linkrow {
                        input type="text" readonly value=(invite_url) aria-label="Invite link";
                        button.btn.primary type="button" data-copy=(invite_url) { "Copy" }
                    }
                    details {
                        summary.link { "Show QR code" }
                        div.qr { (PreEscaped(qr_svg(&invite_url))) }
                    }
                }

                section.panel.step #people data-poll=(admin_path) data-poll-ids="count chips" {
                    div.stephead {
                        span.num aria-hidden="true" { "2" }
                        h2 { "Who’s in " span.count #count { "· " (n) } }
                    }
                    ul.chips #chips {
                        @for m in &g.members { li title=(m.name) { (m.name) @if m.host { " (you)" } } }
                        @if g.members.is_empty() { li.empty { "Nobody yet" } }
                    }
                    p.muted { "New people appear here as they join." }
                    @if g.members.iter().any(|m| !m.host) {
                        details {
                            summary.link { "Edit list" }
                            ul.rows {
                                @for m in g.members.iter().filter(|m| !m.host) {
                                    li {
                                        span.name title=(m.name) { (m.name) }
                                        form method="post" action=(format!("{admin_path}/people/{}/delete", m.id)) data-confirm=(format!("Remove {} from the group?", m.name)) {
                                            button.btn.small type="submit" aria-label=(format!("Remove {}", m.name)) { "Remove" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                section.panel.step #draw {
                    div.stephead { span.num aria-hidden="true" { "3" } h2 { "Draw names" } }
                    details.disclose #rules open[rules_open] {
                        summary { (rules_label) }
                        div.inset {
                            p.muted { "Stop someone drawing a particular person: partners, housemates, last year’s match." }
                            @if n >= 2 {
                                form.stack method="post" action=(format!("{admin_path}/rules")) {
                                    div.rulegrid {
                                        label.field {
                                            "Giver"
                                            select name="giver" {
                                                @for m in &g.members { option value=(m.id) { (m.name) } }
                                            }
                                        }
                                        span.cant aria-hidden="true" { (icon(ONE_WAY)) }
                                        label.field {
                                            "Can’t give to"
                                            select name="receiver" {
                                                @for (k, m) in g.members.iter().enumerate() { option value=(m.id) selected[k == 1] { (m.name) } }
                                            }
                                        }
                                    }
                                    div.ruleactions {
                                        label.check.plain {
                                            input type="checkbox" name="both" value="1" checked;
                                            "Both ways"
                                        }
                                        button.btn type="submit" { "Add rule" }
                                    }
                                }
                            } @else {
                                p.muted { "Rules can be added once two people have joined." }
                            }
                            @if !g.rules.is_empty() {
                                ul.rules {
                                    @for (i, r) in g.rules.iter().enumerate() {
                                        li {
                                            span.pair {
                                                span title=(name_of(r.giver)) { (name_of(r.giver)) }
                                                @if r.both {
                                                    span role="img" aria-label="can’t give to each other" { (icon(BOTH_WAYS)) }
                                                } @else {
                                                    span role="img" aria-label="can’t give to" { (icon(ONE_WAY)) }
                                                }
                                                span title=(name_of(r.receiver)) { (name_of(r.receiver)) }
                                            }
                                            form method="post" action=(format!("{admin_path}/rules/{i}/delete")) {
                                                button.iconbtn type="submit" aria-label=(format!("Remove rule {} and {}", name_of(r.giver), name_of(r.receiver))) { (icon(CROSS)) }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    @match &problem {
                        None => p.ok {
                            (icon(CHECK))
                            span { "Ready to draw: " (plural(n, "person", "people")) @if !g.rules.is_empty() { ", " (plural(g.rules.len(), "rule", "rules")) } "." }
                        },
                        Some((title, text)) => div.problem role="alert" {
                            (icon(WARN))
                            span { strong { (title) } (text) }
                        },
                    }
                    @if host_can_deduce {
                        p.warn { "The group is small enough that once you see your own tag, you could work out who someone else got. Invite more people to keep it a surprise." }
                    }
                    form method="post" action=(format!("{admin_path}/draw")) data-confirm="Draw names now? Joining closes and everyone gets their gift tag." {
                        button.btn.primary.big type="submit" disabled[!ready] { "Draw names" }
                    }
                    p.muted.small { "Joining closes when you draw. You won’t see who got who." }
                }
            }
        },
    )
}

fn join_page(g: &Group, returning: Option<&Member>, f: &JoinForm, error: Option<&str>) -> Markup {
    let n = g.members.len();
    layout(
        &format!("Join {} · Kringle", g.name),
        html! {},
        html! {
            section.invite {
                span.hole aria-hidden="true" {}
                span.from { (g.host) " invited you to" }
                h1 { (g.name) }
                @if !g.budget.is_empty() { div.pills { span { "$" (g.budget) " budget" } } }
                @if n > 0 {
                    div.faces {
                        span.f aria-hidden="true" { @for m in g.members.iter().take(6) { span { (initials(&m.name)) } } }
                        span { (n) " already in the hat" }
                    }
                }
            }
            @if let Some(m) = returning {
                section.panel {
                    h2 { "Welcome back, " (m.name) }
                    p { "You’ve already joined this group on this device." }
                    a.btn.primary.big href=(format!("/p/{}", m.token)) { "Open my page" }
                }
            } @else if g.drawn.is_some() {
                section.panel {
                    h2 { "Names have been drawn" }
                    p { "Joining is closed for this group. If you joined earlier, open your personal link, or reopen this invite on the phone you joined with." }
                }
            } @else {
                form.panel.stack method="post" action=(format!("/j/{}", g.invite)) {
                    @if let Some(e) = error { p.error role="alert" { (e) } }
                    label.field {
                        "Your name"
                        input type="text" name="name" maxlength=(NAME_MAX) required placeholder="Priya" autocomplete="given-name" value=(f.name);
                        span.hint { "Use the name the group knows you by. The host uses it to set “keep apart” rules." }
                    }
                    label.field {
                        span.row { "Wishes & hints" span.hint { "optional" } }
                        textarea name="wishes" rows="4" maxlength=(WISHES_MAX) placeholder="Sizes, favourite things, allergies, “please, no more socks”…" { (f.wishes) }
                    }
                    button.btn.primary.big type="submit" { (icon(GIFT_ICON)) "Put my name in the hat" }
                    p.muted.center { "No sign-up. You’ll get a personal link where your gift tag appears once " (first_word(&g.host)) " draws names." }
                }
            }
        },
    )
}

fn wishes_editor(m: &Member, back: &str) -> Markup {
    html! {
        details {
            summary.link { @if m.wishes.is_empty() { "Add wishes" } @else { "Edit my wishes" } }
            form.stack method="post" action=(format!("/p/{}/wishes", m.token)) {
                input type="hidden" name="back" value=(back);
                label.field {
                    "Wishes & hints"
                    textarea name="wishes" rows="5" maxlength=(WISHES_MAX) { (m.wishes) }
                }
                button.btn type="submit" { "Save wishes" }
            }
        }
    }
}

fn waiting_page(g: &Group, m: &Member, base: &str) -> Markup {
    let my_url = format!("{base}/p/{}", m.token);
    let host_first = first_word(&g.host);
    layout(
        &format!("{} · Kringle", g.name),
        html! { span.tagline { (g.name) } },
        html! {
            div.meintro {
                (icon(GIFT_BIG))
                h1 { "You’re in the hat, " em { (m.name) "!" } }
                p.onsky-muted {
                    @if m.host { "You’re hosting. Draw names from your admin page when everyone’s in." }
                    @else { (host_first) " will draw names soon. This page updates by itself." }
                }
            }
            div.grid2 {
                div {
                    section.panel.gold {
                        h2 { "Your personal link" }
                        div.linkrow {
                            input type="text" readonly value=(my_url) aria-label="Your personal link";
                            button.btn.primary type="button" data-copy=(my_url) { "Copy" }
                        }
                        p { "Bookmark it. It’s your way back to your gift tag, and to edit your wishes." }
                    }
                    section.panel {
                        h2 { "What happens next" }
                        ol.timeline {
                            li { span.dot.done { (icon(CHECK)) } span { strong { "You joined" } } }
                            li { span.dot.now {} span { strong { (host_first) " draws names" } br; span.muted { "Waiting… " (plural(g.members.len(), "person", "people")) " in so far" } } }
                            li { span.dot.later { (icon(GIFT_ICON)) } span { strong.muted { "Unwrap your gift tag" } br; span.muted { "Right here, on this page" } } }
                        }
                    }
                }
                div {
                    section.panel #wishes {
                        h2 { "Your wishes" }
                        @if m.wishes.is_empty() { p.muted { "No wishes yet. A few hints help whoever draws you." } }
                        @else { p.wishtext { (m.wishes) } }
                        (wishes_editor(m, "me"))
                        p.muted.small { "Only whoever draws you will see this." }
                    }
                    section.panel {
                        h2 { "Also in the hat" }
                        ul.chips {
                            @for o in g.members.iter().filter(|o| o.id != m.id) { li title=(o.name) { (o.name) } }
                            @if g.members.len() < 2 { li.empty { "Just you so far" } }
                        }
                    }
                }
            }
            div hidden data-reload-when=(format!("/p/{}/state", m.token)) {}
        },
    )
}

fn tag_page(g: &Group, m: &Member, to: &Member, open: bool) -> Markup {
    let full = to.name.chars().count() > TAG_NAME_FULL;
    let big = if full {
        first_word(&to.name)
    } else {
        to.name.as_str()
    };
    let size = match big.chars().count() {
        0..=6 => "xl",
        7..=9 => "l",
        10..=12 => "m",
        _ => "s",
    };
    let wishes_len = to.wishes.chars().count();
    let long = wishes_len > WISHES_PREVIEW;
    layout(
        &format!("Your gift tag · {}", g.name),
        html! { span.hi title=(m.name) { "Hi, " (m.name) } },
        html! {
            div.tagpage {
                span.plate { (g.name) }
                @if !open {
                    h1.onsky { "Your gift tag is ready" }
                    (icon(GIFT_BIG_TILTED))
                    p.plate.small { "Make sure nobody’s peeking over your shoulder." }
                    form method="post" action=(format!("/p/{}/open", m.token)) {
                        button.btn.primary.big.unwrap type="submit" { (icon(GIFT_ICON)) "Unwrap my tag" }
                    }
                } @else {
                    h1.onsky { "You’re buying for…" }
                    div.tagwrap #tag {
                        (icon(TAG_STRING))
                        div.tag {
                            span.hole aria-hidden="true" {}
                            span.to { "To" }
                            span class=(format!("who {size}")) { (big) }
                            @if full { span.full { (to.name) } }
                            div.stripe aria-hidden="true" {}
                            span.label { (first_word(&to.name)) "’s wishes" }
                            @if to.wishes.is_empty() {
                                p.muted { "No wishes yet. They can add some any time, so check back closer to the day." }
                            } @else if long {
                                p.wish-body { (to.wishes.chars().take(WISHES_PREVIEW).collect::<String>()) "…" }
                                details.more {
                                    summary.link { "Show all wishes" }
                                    p.wish-body { (to.wishes) }
                                }
                            } @else {
                                p class=(if wishes_len <= 90 { "wish-hand" } else { "wish-hand smaller" }) { "“" (to.wishes) "”" }
                            }
                            @if !g.budget.is_empty() { span.pill.gold-pill { "Up to $" (g.budget) } }
                            span.from { "From: " em { "you (shh!)" } }
                        }
                    }
                    div.actions {
                        a.btn href=(format!("/p/{}", m.token)) { "Hide it again" }
                    }
                    section.panel.narrow #wishes {
                        h2 { "Your own wishes" }
                        @if m.wishes.is_empty() { p.muted { "You haven’t added any. Whoever drew you would love a hint." } }
                        @else { p.wishtext { (m.wishes) } }
                        (wishes_editor(m, "tag"))
                    }
                    p.plate.small { "Keep it secret. " (first_word(&to.name)) " sees a tag for someone else. Not even " (first_word(&g.host)) " knows who you got." }
                }
            }
        },
    )
}

// ---------------------------------------------------------------------------
// Icons and illustrations
// ---------------------------------------------------------------------------

const LOGO: &str = r##"<svg viewBox="0 0 120 120" aria-hidden="true"><rect x="18" y="52" width="84" height="58" rx="8" fill="#9B2C1F"/><rect x="12" y="36" width="96" height="22" rx="6" fill="#9B2C1F"/><rect x="52" y="36" width="16" height="74" fill="#C8962F"/><path d="M60 36 C44 10 20 20 32 32 C38 38 50 36 60 36 Z" fill="#C8962F"/><path d="M60 36 C76 10 100 20 88 32 C82 38 70 36 60 36 Z" fill="#C8962F"/></svg>"##;

const GIFT_BIG: &str = r##"<svg class="gift" viewBox="0 0 120 120" aria-hidden="true"><rect x="18" y="52" width="84" height="58" rx="8" fill="#9B2C1F"/><circle cx="34" cy="70" r="4" fill="#E6D6B6" opacity="0.5"/><circle cx="86" cy="92" r="4" fill="#E6D6B6" opacity="0.5"/><circle cx="36" cy="98" r="4" fill="#E6D6B6" opacity="0.5"/><rect x="12" y="36" width="96" height="22" rx="6" fill="#9B2C1F"/><rect x="12" y="54" width="96" height="4" fill="#000" opacity="0.12"/><rect x="52" y="36" width="16" height="74" fill="#C8962F"/><path d="M60 36 C44 10 20 20 32 32 C38 38 50 36 60 36 Z" fill="#C8962F"/><path d="M60 36 C76 10 100 20 88 32 C82 38 70 36 60 36 Z" fill="#C8962F"/></svg>"##;

const GIFT_BIG_TILTED: &str = r##"<svg class="giftbig" viewBox="0 0 120 120" aria-hidden="true"><rect x="18" y="52" width="84" height="58" rx="8" fill="#9B2C1F"/><circle cx="34" cy="70" r="4" fill="#E6D6B6" opacity="0.5"/><circle cx="86" cy="92" r="4" fill="#E6D6B6" opacity="0.5"/><circle cx="36" cy="98" r="4" fill="#E6D6B6" opacity="0.5"/><circle cx="84" cy="66" r="4" fill="#E6D6B6" opacity="0.5"/><rect x="12" y="36" width="96" height="22" rx="6" fill="#9B2C1F"/><rect x="12" y="54" width="96" height="4" fill="#000" opacity="0.12"/><rect x="52" y="36" width="16" height="74" fill="#C8962F"/><path d="M60 36 C44 10 20 20 32 32 C38 38 50 36 60 36 Z" fill="#C8962F"/><path d="M60 36 C76 10 100 20 88 32 C82 38 70 36 60 36 Z" fill="#C8962F"/></svg>"##;

const GIFT_ICON: &str = r##"<svg class="i" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="3" y="8" width="18" height="5" rx="1"/><path d="M5 13v8h14v-8"/><path d="M12 8v13"/><path d="M12 8c-2-4-6-4-6-1.5S9 8 12 8zM12 8c2-4 6-4 6-1.5S15 8 12 8z"/></svg>"##;

const LOCK: &str = r##"<svg class="i" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"><rect x="5" y="11" width="14" height="10" rx="2"/><path d="M8 11V7a4 4 0 0 1 8 0v4"/></svg>"##;

const CHECK: &str = r##"<svg class="i" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M5 12.5l4.5 4.5L19 7.5"/></svg>"##;

const WARN: &str = r##"<svg class="i" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M12 3l10 18H2L12 3z"/><path d="M12 10v5M12 18v.5"/></svg>"##;

const CROSS: &str = r##"<svg class="i" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" aria-hidden="true"><path d="M6 6l12 12M18 6L6 18"/></svg>"##;

/// Two opposite arrows struck through: these two can't give to each other.
const BOTH_WAYS: &str = r##"<svg class="rule-icon" viewBox="0 0 30 22" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M4 7h20M20 3l4 4-4 4"/><path d="M26 15H6M10 11l-4 4 4 4"/><path d="M8 20L22 2" stroke-width="2.6"/></svg>"##;

/// One arrow struck through: the first person can't give to the second.
const ONE_WAY: &str = r##"<svg class="rule-icon" viewBox="0 0 30 22" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M4 11h20M20 7l4 4-4 4"/><path d="M8 20L22 2" stroke-width="2.6"/></svg>"##;

const TAG_STRING: &str = r##"<svg class="string" viewBox="0 0 200 60" preserveAspectRatio="none" aria-hidden="true"><path d="M100 50 C 70 10, 40 30, 10 6" fill="none" stroke="#C8962F" stroke-width="2.5" stroke-linecap="round"/></svg>"##;

// ---------------------------------------------------------------------------
// Stylesheet: "Bethlehem" (limestone panels, etched headings, daytime sky)
// ---------------------------------------------------------------------------

const CSS: &str = r##"
@font-face{font-family:"IM Fell English";src:url(/static/fell.woff2) format("woff2");font-weight:400;font-style:normal;font-display:swap}
@font-face{font-family:"IM Fell English";src:url(/static/fell-italic.woff2) format("woff2");font-weight:400;font-style:italic;font-display:swap}
:root{
  --ink:#2B1A0C;--muted:#4E341C;--stone:#E6D6B6;--stone2:#D6C29C;--field:#FBF6EA;--edge:#B39A70;--line:#A88F68;
  --red:#9B2C1F;--red-ink:#FFF4E0;--red-text:#7A1E12;--olive:#3F4C18;--olive-soft:#D3CF9C;--gold:#C8962F;--gold-soft:#EAD29A;
  --danger:#8A1F14;--danger-soft:#F0CDB8;--sky-ink:#0E1C28;--sky-muted:#1B2E3C;--sky-accent:#6E1A12;
  --display:"IM Fell English",Georgia,serif;--body:Georgia,"Times New Roman",serif;
  --etch:0 1px 0 rgba(255,240,210,.6),0 -1px 0 rgba(60,30,0,.3);--shadow:4px 5px 0 rgba(20,10,0,.5);
  --fall:linear-gradient(180deg,rgba(255,251,240,.45),rgba(255,251,240,0) 45%,rgba(120,96,60,.06));
}
*{box-sizing:border-box}
html{background:#EEF5FA}
body{margin:0;position:relative;min-height:100vh;overflow-x:hidden;color:var(--ink);font-family:var(--body);font-size:16px;line-height:1.45;
  background:#8CBCE6 linear-gradient(to bottom,#5E9BD6 0%,#8CBCE6 40%,#C6E0F2 75%,#EEF5FA 100%) no-repeat;background-size:100% 100%}
:focus-visible{outline:2px dotted var(--gold);outline-offset:3px}
p{margin:0}
a{color:var(--red-text)}
h1,h2{font-family:var(--display);font-weight:400;margin:0;line-height:1.08}
h1{font-size:clamp(34px,6vw,52px);overflow-wrap:anywhere}
h2{font-size:26px}
.panel h1,.panel h2,.drawn-card h2,.invite h1{text-shadow:var(--etch)}
svg.i{width:18px;height:18px;flex-shrink:0}

.sky{position:absolute;left:0;right:0;top:0;height:720px;overflow:hidden;pointer-events:none}
.sun{position:absolute;left:calc(50% + 70px);top:40px;width:150px;height:150px;border-radius:50%;
  background:radial-gradient(circle,#FFF9E6 0 30%,rgba(255,249,230,.5) 45%,rgba(255,249,230,0) 70%)}
.clouds{position:absolute;left:0;top:0;width:100%;height:720px;object-fit:cover;object-position:50% 0}
.town{position:absolute;left:0;bottom:0;width:100%;height:420px;object-fit:cover;object-position:50% 100%;pointer-events:none}

header.site{position:relative;z-index:1;display:flex;align-items:center;justify-content:space-between;gap:16px;padding:14px 20px;background:var(--stone) var(--fall)}
.brand{display:flex;align-items:center;gap:10px;color:var(--ink);text-decoration:none;font-family:var(--display);font-size:28px;text-shadow:var(--etch)}
.brand svg{width:34px;height:34px}
.tagline,.hi{min-width:0;overflow:hidden;white-space:nowrap;text-overflow:ellipsis;font-size:15px;font-weight:700;color:var(--muted)}
.badge{display:inline-flex;align-items:center;gap:6px;padding:5px 12px;border-radius:999px;background:var(--gold-soft);font-size:13px;font-weight:700}
.band{position:relative;z-index:1;height:10px;background:repeating-linear-gradient(-45deg,var(--edge) 0 10px,var(--stone) 10px 20px)}
main{position:relative;z-index:1;width:100%;max-width:720px;margin:0 auto;padding:32px 16px 280px;display:flex;flex-direction:column;gap:20px}

.panel{display:flex;flex-direction:column;gap:14px;padding:24px;background:var(--stone) var(--fall);border:4px groove var(--edge);border-radius:15px;box-shadow:var(--shadow)}
.panel.gold{background:var(--gold-soft);border:2px dashed var(--gold);box-shadow:none}
.stack{display:flex;flex-direction:column;gap:18px}
.row{display:flex;align-items:center;justify-content:space-between;gap:12px}
.aside{flex-shrink:0;font-family:var(--display);font-style:italic;font-size:22px;color:var(--red-text)}
.muted{color:var(--muted)}
.small{font-size:14px}
.center{text-align:center}
.intro{display:flex;flex-direction:column;gap:10px;align-items:flex-start}
.onsky{color:var(--sky-ink)}
.onsky-muted{color:var(--sky-muted);font-weight:700}
.plate{align-self:flex-start;max-width:100%;padding:3px 14px 5px;background:var(--stone) var(--fall);border:4px groove var(--edge);border-radius:10px;
  box-shadow:var(--shadow);font-family:var(--display);font-style:italic;font-size:24px;color:var(--red-text);overflow-wrap:anywhere}
.plate.small{padding:10px 16px;font-family:var(--body);font-style:normal;font-size:15px;color:var(--ink)}
.lock{display:flex;gap:8px;align-items:flex-start;font-size:13px;color:var(--muted)}
.error{padding:10px 14px;border-radius:10px;background:var(--danger-soft);color:var(--danger);font-weight:700}

label.field{display:flex;flex-direction:column;gap:6px;min-width:0;font-size:15px;font-weight:700}
.hint{font-size:13px;font-weight:400;color:var(--muted)}
input[type=text],textarea,select{width:100%;min-width:0;min-height:48px;padding:0 14px;border:2px solid var(--edge);border-radius:8px;background:var(--field);
  box-shadow:inset 0 1px 2px rgba(80,60,30,.18);color:var(--ink);font:inherit;font-size:17px;font-weight:700}
textarea{min-height:120px;padding:12px 14px;line-height:1.45;resize:vertical}
input::placeholder,textarea::placeholder{color:#76624C;font-weight:400;opacity:1}
.money{display:flex;align-items:center;gap:8px}
.money span{font-size:17px;font-weight:700;color:var(--muted)}
.check{display:flex;align-items:center;gap:12px;min-height:48px;padding:0 14px;border-radius:10px;background:var(--gold-soft);font-weight:700;cursor:pointer}
.check.plain{padding:0;background:none}
.check input{width:20px;height:20px;margin:0;accent-color:var(--red)}

.btn{display:inline-flex;align-items:center;justify-content:center;gap:10px;min-height:44px;padding:0 16px;border:3px outset #CDB68C;border-radius:10px;
  background:var(--stone) var(--fall);color:var(--ink);font:inherit;font-size:15px;font-weight:700;text-decoration:none;cursor:pointer}
.btn.primary{border-color:#C0493A;background:var(--red);color:var(--red-ink)}
.btn.big{width:100%;min-height:58px;font-size:19px}
.btn.small{min-height:40px;padding:0 12px;font-size:14px}
.btn:disabled{border:3px solid var(--line);background:var(--stone2);color:var(--muted);cursor:not-allowed}
.iconbtn{flex-shrink:0;width:44px;height:44px;display:flex;align-items:center;justify-content:center;border:0;border-radius:10px;background:transparent;color:var(--muted);cursor:pointer}

summary{cursor:pointer;list-style:none}
summary::-webkit-details-marker{display:none}
summary.link{display:inline-flex;align-items:center;min-height:44px;color:var(--red-text);font-weight:700;text-decoration:underline}
.disclose>summary{display:flex;align-items:center;justify-content:space-between;gap:12px;min-height:48px;padding:0 14px;border:3px outset #CDB68C;border-radius:10px;
  background:var(--stone2) var(--fall);font-weight:700}
.disclose>summary::after{content:"";width:9px;height:9px;margin-top:-4px;border-right:2.5px solid currentColor;border-bottom:2.5px solid currentColor;transform:rotate(45deg)}
.disclose[open]>summary::after{margin-top:4px;transform:rotate(-135deg)}
.inset{display:flex;flex-direction:column;gap:12px;margin-top:12px;padding:14px;border:2px solid var(--edge);border-radius:10px;background:var(--stone2) var(--fall)}

.stephead{display:flex;align-items:center;gap:12px}
.num{flex-shrink:0;width:34px;height:34px;display:flex;align-items:center;justify-content:center;border:2px outset #F0D48A;border-radius:10px;background:var(--gold);font-family:var(--display);font-size:20px}
.count{font-family:var(--body);font-size:16px;color:var(--muted)}
.adminlink{display:flex;align-items:center;gap:12px;flex-wrap:wrap;padding:10px 14px;border:2px dashed var(--gold);border-radius:10px;background:var(--gold-soft)}
.adminlink p{flex:1 1 220px;font-size:14px}
.linkrow{display:flex;gap:8px;flex-wrap:wrap}
.linkrow input{flex:1 1 220px;width:auto;font-size:15px}
.qr{margin-top:8px}
.qr svg{display:block;width:200px;height:200px;border:2px solid var(--edge);border-radius:8px}

.chips{list-style:none;margin:0;padding:0;display:flex;flex-wrap:wrap;gap:8px}
.chips li{max-width:100%;overflow:hidden;white-space:nowrap;text-overflow:ellipsis;padding:6px 12px;border:1px solid var(--line);border-radius:999px;background:var(--stone2);font-size:15px;font-weight:700}
.chips li.empty{border-style:dashed;background:none;font-weight:400;color:var(--muted)}
.rows{list-style:none;margin:8px 0 0;padding:0}
.rows li{display:flex;align-items:center;gap:10px;min-height:48px;border-top:1px solid var(--line)}
.rows .name{flex:1 1 auto;min-width:0;overflow:hidden;white-space:nowrap;text-overflow:ellipsis;font-weight:700}
.pill{display:inline-block;padding:4px 10px;border-radius:999px;background:var(--stone2);color:var(--muted);font-size:13px;font-weight:700}
.pill.yes{background:var(--olive-soft);color:var(--olive)}

.rulegrid{display:grid;grid-template-columns:minmax(0,1fr) auto minmax(0,1fr);gap:12px 14px;align-items:end}
.cant{height:48px;display:flex;align-items:center;color:var(--danger)}
.ruleactions{display:flex;align-items:center;justify-content:space-between;gap:10px;flex-wrap:wrap}
.rules{list-style:none;margin:0;padding:0;display:flex;flex-direction:column;gap:8px}
.rules li{display:flex;align-items:center;gap:10px;min-height:48px;padding:2px 4px 2px 12px;border:1px solid var(--line);border-radius:10px;background:var(--stone)}
.pair{flex:1 1 auto;min-width:0;display:flex;align-items:center;gap:8px;font-weight:700}
.pair>span{min-width:0;overflow:hidden;white-space:nowrap;text-overflow:ellipsis}
.pair>span[role=img]{flex-shrink:0;display:flex;overflow:visible}
.rule-icon{width:30px;height:22px;color:var(--danger)}
.ok{display:flex;gap:10px;align-items:flex-start;font-size:15px}
.ok svg{color:var(--olive)}
.warn{padding:10px 12px;border-radius:10px;background:var(--gold-soft);font-size:14px}
.problem{display:flex;gap:10px;align-items:flex-start;padding:12px 14px;border-radius:10px;background:var(--danger-soft);font-size:15px}
.problem svg{color:var(--danger)}
.problem strong{display:block;color:var(--danger)}

.drawn-card{display:flex;flex-direction:column;gap:14px;padding:24px;border:4px groove #6E1D14;border-radius:15px;background:var(--red);color:var(--red-ink);box-shadow:var(--shadow)}
.drawn-card h2{font-size:34px;text-shadow:0 -1px 0 rgba(0,0,0,.35)}
.kicker{font-size:15px;font-weight:700}
.bar{height:14px;overflow:hidden;border:2px inset #6E1D14;border-radius:10px;background:rgba(0,0,0,.25)}
.bar span{display:block;height:100%;background:var(--gold)}

.invite{position:relative;display:flex;flex-direction:column;align-items:center;gap:12px;padding:30px 24px 24px;text-align:center;
  border:4px groove #6E1D14;border-radius:15px;background:var(--red);color:var(--red-ink);box-shadow:var(--shadow)}
.invite h1{text-shadow:0 -1px 0 rgba(0,0,0,.35)}
.invite .hole{position:absolute;top:12px;left:50%;width:14px;height:14px;margin-left:-7px;border-radius:50%;background:#A9CEEC}
.invite .from{margin-top:8px;font-family:var(--display);font-style:italic;font-size:24px}
.pills span{display:inline-block;padding:6px 12px;border:1.5px solid var(--red-ink);border-radius:999px;font-size:14px;font-weight:700}
.faces{display:flex;align-items:center;justify-content:center;gap:10px;flex-wrap:wrap;font-weight:700}
.faces .f{display:flex}
.faces .f span{width:32px;height:32px;margin-left:-8px;display:flex;align-items:center;justify-content:center;border:2px solid var(--red);border-radius:50%;
  background:var(--gold-soft);color:var(--ink);font-size:12px}
.faces .f span:first-child{margin-left:0}

.meintro{display:flex;flex-direction:column;gap:12px;align-items:flex-start;color:var(--sky-ink)}
.meintro h1 em{color:var(--sky-accent)}
.meintro .gift{width:100px;height:100px}
.grid2{display:flex;flex-direction:column;gap:20px}
.grid2>div{display:flex;flex-direction:column;gap:20px}
.timeline{list-style:none;margin:0;padding:0;display:flex;flex-direction:column;gap:16px}
.timeline li{display:flex;gap:14px}
.dot{flex-shrink:0;width:32px;height:32px;display:flex;align-items:center;justify-content:center;border-radius:50%}
.dot.done{background:var(--olive);color:var(--red-ink)}
.dot.now{border:2.5px solid var(--red-text)}
.dot.now::after{content:"";width:12px;height:12px;border-radius:50%;background:var(--red-text)}
.dot.later{background:var(--stone2);color:var(--muted)}
.wishtext{font-family:var(--display);font-style:italic;font-size:24px;line-height:1.25;white-space:pre-line;overflow-wrap:anywhere}

.tagpage{display:flex;flex-direction:column;align-items:center;gap:24px;text-align:center}
.tagpage .plate{align-self:center}
.giftbig{width:min(260px,70vw);height:auto;transform:rotate(-4deg)}
.unwrap{width:auto;min-width:280px;padding:0 28px}
.tagwrap{position:relative;width:min(460px,100%);margin-top:32px;transform:rotate(-2.5deg);filter:drop-shadow(0 18px 24px rgba(20,10,0,.45))}
.tagwrap .string{position:absolute;left:0;top:-44px;width:100%;height:60px}
.tag{position:relative;display:flex;flex-direction:column;align-items:center;gap:14px;padding:64px 36px 32px;clip-path:polygon(50% 0,100% 16%,100% 100%,0 100%,0 16%);
  border-radius:0 0 18px 18px;background:var(--stone) var(--fall)}
.tag .hole{position:absolute;top:22px;left:50%;width:18px;height:18px;margin-left:-9px;border:3px solid var(--gold);border-radius:50%;background:#A9CEEC}
.tag .to{font-size:14px;font-weight:700;letter-spacing:2px;text-transform:uppercase;color:var(--muted)}
.who{max-width:100%;font-family:var(--display);font-style:italic;line-height:.95;color:var(--red-text);text-shadow:var(--etch);overflow-wrap:anywhere}
.who.xl{font-size:106px}.who.l{font-size:83px}.who.m{font-size:70px}.who.s{font-size:58px}
.tag .full{margin-top:-6px;font-size:15px;font-weight:700;color:var(--muted);overflow-wrap:anywhere}
.tag .stripe{width:100%;height:8px;border-radius:4px;background:repeating-linear-gradient(-45deg,var(--edge) 0 8px,var(--stone) 8px 16px)}
.tag .label{max-width:100%;overflow:hidden;white-space:nowrap;text-overflow:ellipsis;font-size:13px;font-weight:700;letter-spacing:1.5px;text-transform:uppercase;color:var(--muted)}
.wish-hand{font-family:var(--display);font-style:italic;font-size:30px;line-height:1.15;white-space:pre-line;overflow-wrap:anywhere}
.wish-hand.smaller{font-size:24px}
.wish-body{width:100%;text-align:left;white-space:pre-line;overflow-wrap:anywhere;line-height:1.5}
.more{width:100%}
.more[open]>p.wish-body{margin-top:8px}
.gold-pill{background:var(--gold-soft);color:var(--ink)}
.tag .from{font-size:14px;font-weight:700;color:var(--muted)}
.tag .from em{font-family:var(--display);font-size:22px}
.actions{display:flex;gap:10px;flex-wrap:wrap;justify-content:center}
.panel.narrow{width:100%;text-align:left}

@media (min-width:900px){
  main:has(.grid2){max-width:1120px}
  .grid2{display:grid;grid-template-columns:minmax(0,1fr) minmax(0,1fr);align-items:start}
}
@media (max-width:560px){
  .panel{padding:18px}
  .tagline{display:none}
  .rulegrid{grid-template-columns:minmax(0,1fr) minmax(0,1fr)}
  .cant{display:none}
  .tag{padding:64px 22px 32px}
  .who.xl{font-size:77px}.who.l{font-size:61px}.who.m{font-size:48px}.who.s{font-size:38px}
  .wish-hand{font-size:26px}
  .unwrap{min-width:0;width:100%}
}
"##;

// ---------------------------------------------------------------------------
// The only JavaScript: copy buttons, confirmations and polling. Everything
// works without it; it just saves some taps and refreshes.
// ---------------------------------------------------------------------------

const JS: &str = r##""use strict";
(function () {
  function copy(text, button) {
    function done() {
      var old = button.textContent;
      button.textContent = "Copied!";
      setTimeout(function () { button.textContent = old; }, 1600);
    }
    function fallback() {
      var t = document.createElement("textarea");
      t.value = text;
      t.setAttribute("readonly", "");
      t.style.position = "fixed";
      t.style.opacity = "0";
      document.body.appendChild(t);
      t.select();
      try { document.execCommand("copy"); done(); } catch (e) {}
      t.remove();
    }
    // Routers usually serve plain http, where the clipboard API is off.
    if (navigator.clipboard && window.isSecureContext) navigator.clipboard.writeText(text).then(done, fallback);
    else fallback();
  }
  document.addEventListener("click", function (e) {
    var b = e.target.closest("[data-copy]");
    if (b) { e.preventDefault(); copy(b.getAttribute("data-copy"), b); }
    if (e.target.matches("input[readonly]")) e.target.select();
  });
  document.addEventListener("submit", function (e) {
    var f = e.target.closest("form[data-confirm]");
    if (f && !window.confirm(f.getAttribute("data-confirm"))) e.preventDefault();
  });
  function every(ms, fn) { setInterval(function () { if (!document.hidden) fn(); }, ms); }
  // Swap in fresh copies of a few elements (e.g. who's joined) from the same page.
  document.querySelectorAll("[data-poll]").forEach(function (el) {
    var url = el.getAttribute("data-poll"), ids = el.getAttribute("data-poll-ids").split(" ");
    every(10000, function () {
      fetch(url, { cache: "no-store" }).then(function (r) { return r.ok ? r.text() : null; }).then(function (html) {
        if (!html) return;
        var doc = new DOMParser().parseFromString(html, "text/html");
        ids.forEach(function (id) {
          var cur = document.getElementById(id), next = doc.getElementById(id);
          if (cur && next) cur.replaceWith(document.importNode(next, true));
        });
      }).catch(function () {});
    });
  });
  // Reload once names are drawn so the gift tag appears.
  document.querySelectorAll("[data-reload-when]").forEach(function (el) {
    var url = el.getAttribute("data-reload-when");
    every(10000, function () {
      fetch(url, { cache: "no-store" }).then(function (r) { if (r.status === 200) location.reload(); }).catch(function () {});
    });
  });
})();
"##;

// ---------------------------------------------------------------------------
// End-to-end tests over the real router
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    fn test_app() -> (Router, PathBuf) {
        let path = std::env::temp_dir().join(format!("kringle-test-{}.json", token()));
        let app = Arc::new(App {
            store: Mutex::new(Store::load(path.clone()).unwrap()),
            public_url: Some("http://kringle.test".into()),
            keep_secs: 86_400,
        });
        (router(app), path)
    }

    struct Res {
        status: StatusCode,
        location: Option<String>,
        set_cookie: Option<String>,
        content_type: String,
        body: String,
    }

    impl Res {
        /// Where a redirect goes, without the #fragment (not part of a request).
        fn to(&self) -> String {
            let loc = self.location.as_deref().expect("a redirect");
            loc.split('#').next().unwrap().to_string()
        }
        fn has(&self, text: &str) -> bool {
            self.body.contains(text)
        }
    }

    async fn req(r: &Router, method: &str, uri: &str, form: &str, cookie: Option<&str>) -> Res {
        let mut b = Request::builder().method(method).uri(uri);
        if method == "POST" {
            b = b.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        }
        if let Some(c) = cookie {
            b = b.header(header::COOKIE, c);
        }
        let res = r
            .clone()
            .oneshot(b.body(Body::from(form.to_string())).unwrap())
            .await
            .unwrap();
        let header = |k| {
            res.headers()
                .get(k)
                .map(|v: &HeaderValue| v.to_str().unwrap().to_string())
        };
        let (location, set_cookie) = (header(header::LOCATION), header(header::SET_COOKIE));
        let content_type = header(header::CONTENT_TYPE).unwrap_or_default();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        Res {
            status,
            location,
            set_cookie,
            content_type,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    async fn get(r: &Router, uri: &str) -> Res {
        req(r, "GET", uri, "", None).await
    }

    async fn post(r: &Router, uri: &str, form: &str) -> Res {
        req(r, "POST", uri, form, None).await
    }

    fn between<'a>(s: &'a str, start: &str, end: &str) -> &'a str {
        let i = s.find(start).unwrap() + start.len();
        &s[i..i + s[i..].find(end).unwrap()]
    }

    /// Start a group and return (admin path, invite path).
    async fn start(r: &Router, form: &str) -> (String, String) {
        let admin = post(r, "/groups", form).await.to();
        let page = get(r, &admin).await;
        let invite = format!("/j/{}", between(&page.body, "http://kringle.test/j/", "\""));
        (admin, invite)
    }

    async fn join(r: &Router, invite: &str, form: &str) -> String {
        let res = post(r, invite, form).await;
        assert_eq!(
            res.status,
            StatusCode::SEE_OTHER,
            "join {form}: {}",
            res.body
        );
        res.to()
    }

    #[tokio::test]
    async fn every_page_and_action() {
        let (r, path) = test_app();

        // Start a group: the form, both validation errors, then success.
        let res = get(&r, "/").await;
        assert!(
            res.has("Start a gift swap") && res.has("checked"),
            "host plays by default"
        );
        assert!(
            post(&r, "/groups", "group=&host=Jamie")
                .await
                .has("Give the group a name.")
        );
        assert!(
            post(&r, "/groups", "group=Flat+4B&host=+")
                .await
                .has("Add your name so people know")
        );
        let (admin, invite) = start(&r, "group=Flat+4B&host=Jamie&budget=%2425abc&plays=1").await;
        let res = get(&r, &admin).await;
        assert!(res.has("$25 budget"), "budget keeps digits only");
        assert!(
            res.has("Hosted by Jamie")
                && res.has("Your own page")
                && res.has("Not enough people yet")
        );
        assert!(
            res.has("Show QR code") && res.has("<svg"),
            "QR code is rendered"
        );

        // Join: the open form, a validation error, success with a cookie, welcome back.
        let res = get(&r, &invite).await;
        assert!(res.has("Jamie invited you to") && res.has("Put my name in the hat"));
        assert!(
            post(&r, &invite, "name=++")
                .await
                .has("Add your name so the host knows")
        );
        let res = post(&r, &invite, "name=Alex&wishes=Hot+sauce").await;
        let alex = res.to();
        let cookie = res
            .set_cookie
            .as_deref()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        assert!(res.set_cookie.unwrap().contains(&format!("Path={invite}")));
        let res = req(&r, "GET", &invite, "", Some(&cookie)).await;
        assert!(
            res.has("Welcome back, Alex") && res.has(&alex),
            "same phone gets back in"
        );
        let sam = join(&r, &invite, "name=Sam").await;
        let priya = join(&r, &invite, "name=Priya").await;

        // The waiting page and editing wishes.
        let res = get(&r, &sam).await;
        assert!(
            res.has("You’re in the hat, <em>Sam!")
                && res.has("No wishes yet")
                && res.has("Also in the hat")
        );
        assert!(res.has(&format!("data-reload-when=\"{sam}/state\"")));
        let res = post(
            &r,
            &format!("{sam}/wishes"),
            "wishes=Board+games%0D%0AAnything+green&back=me",
        )
        .await;
        assert!(res.location.unwrap().ends_with("#wishes"));
        assert!(get(&r, &sam).await.has("Board games\nAnything green"));

        // Rules: add both ways and one way, refuse duplicates and self-rules, remove.
        // Member ids: Jamie 1, Alex 2, Sam 3, Priya 4.
        let rules = format!("{admin}/rules");
        assert_eq!(
            post(&r, &rules, "giver=2&receiver=3&both=1").await.to(),
            admin
        );
        assert!(
            get(&r, &admin)
                .await
                .has("aria-label=\"can’t give to each other\"")
        );
        let dup = post(&r, &rules, "giver=3&receiver=2").await.to();
        assert!(dup.ends_with("?err=dup") && get(&r, &dup).await.has("That rule already exists."));
        let same = post(&r, &rules, "giver=2&receiver=2").await.to();
        assert!(get(&r, &same).await.has("Pick two different people."));
        post(&r, &rules, "giver=4&receiver=1").await;
        let res = get(&r, &admin).await;
        assert!(
            res.has("aria-label=\"can’t give to\"") && res.has("Keep some people apart · 2 rules")
        );
        post(&r, &format!("{admin}/rules/1/delete"), "").await;
        assert!(!get(&r, &admin).await.has("aria-label=\"can’t give to\""));

        // Remove a person: their rules go too; the host can't be removed.
        post(&r, &format!("{admin}/people/3/delete"), "").await;
        post(&r, &format!("{admin}/people/1/delete"), "").await;
        let res = get(&r, &admin).await;
        assert!(!res.has("title=\"Sam\"") && res.has("Jamie (you)"));
        assert!(
            !res.has("can’t give to each other") && res.has("Keep some people apart (optional)")
        );
        assert_eq!(get(&r, &sam).await.status, StatusCode::NOT_FOUND);
        assert!(
            res.has("Ready to draw: 3 people.") && res.has("small enough"),
            "host could deduce"
        );

        // Draw: everything locks, tags appear.
        assert_eq!(
            get(&r, &format!("{priya}/state")).await.status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(post(&r, &format!("{admin}/draw"), "").await.to(), admin);
        assert_eq!(
            get(&r, &format!("{priya}/state")).await.status,
            StatusCode::OK
        );
        let res = get(&r, &admin).await;
        assert!(
            res.has("Gift tags are out") && res.has("0 of 3 have opened") && res.has("Not yet")
        );
        assert!(get(&r, &rules).await.status == StatusCode::METHOD_NOT_ALLOWED);
        assert!(
            post(&r, &rules, "giver=2&receiver=4")
                .await
                .to()
                .ends_with("?err=drawn")
        );
        assert!(
            post(&r, &format!("{admin}/draw"), "")
                .await
                .to()
                .ends_with("?err=drawn")
        );
        assert!(
            post(&r, &invite, "name=Late")
                .await
                .has("Names have been drawn"),
            "joining is closed"
        );
        assert!(get(&r, &invite).await.has("Names have been drawn"));

        // Unwrap, hide, edit wishes from the tag.
        let res = get(&r, &priya).await;
        assert!(res.has("Your gift tag is ready") && res.has("Unwrap my tag"));
        assert!(
            get(&r, &format!("{priya}?show=1"))
                .await
                .has("Your gift tag is ready"),
            "must unwrap first"
        );
        let shown = post(&r, &format!("{priya}/open"), "").await.to();
        let res = get(&r, &shown).await;
        assert!(
            res.has("You’re buying for") && res.has("From: <em>you (shh!)") && res.has("Up to $25")
        );
        assert!(res.has("Hide it again") && res.has("Not even Jamie knows"));
        let back = post(&r, &format!("{priya}/wishes"), "wishes=Tea&back=tag").await;
        assert!(back.location.unwrap().ends_with("?show=1#tag"));
        assert!(get(&r, &shown).await.has("Tea"));
        assert!(get(&r, &admin).await.has("1 of 3 have opened"));

        // Cancel the draw: joining reopens and tags disappear.
        assert_eq!(post(&r, &format!("{admin}/undraw"), "").await.to(), admin);
        assert!(get(&r, &admin).await.has("Invite people"));
        assert!(get(&r, &priya).await.has("You’re in the hat"));
        assert_eq!(
            get(&r, &format!("{priya}/state")).await.status,
            StatusCode::NO_CONTENT
        );
        assert!(get(&r, &invite).await.has("Put my name in the hat"));

        // Everything survives a restart.
        let reloaded = Store::load(path.clone()).unwrap();
        assert_eq!(reloaded.data.groups[0].members.len(), 3);
        assert_eq!(reloaded.data.groups[0].members[2].wishes, "Tea");
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn impossible_draw_is_blocked() {
        let (r, path) = test_app();
        let (admin, invite) = start(&r, "group=Flat&host=Jo").await;
        for name in ["Alex", "Sam", "Kim"] {
            join(&r, &invite, &format!("name={name}")).await;
        }
        // Host isn't playing, so members are Alex 1, Sam 2, Kim 3.
        post(&r, &format!("{admin}/rules"), "giver=1&receiver=2&both=1").await;
        let res = get(&r, &admin).await;
        assert!(res.has("No fair draw is possible") && res.has("disabled"));
        assert!(
            res.has("<details class=\"disclose\" id=\"rules\" open"),
            "rules open when they are the problem"
        );
        let to = post(&r, &format!("{admin}/draw"), "").await.to();
        assert!(get(&r, &to).await.has("Names couldn’t be drawn"));
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn long_names_and_wishes() {
        let (r, path) = test_app();
        let (admin, invite) = start(&r, "group=The+Featherstonehaugh%E2%80%93Okonkwo+Family+Christmas+Extravaganza+2026+and+more&host=Ana").await;
        let long_wish = "Anything for the garden. ".repeat(30);
        let names = [
            "Guinevere Ottoline Beauchamp-Ravensworth the Second",
            "Maximilian Featherstonehaugh",
            "Bartholomew J. Winterbottom III",
        ];
        let mut links = Vec::new();
        for name in names {
            let form = format!(
                "name={}&wishes={}",
                name.replace(' ', "+"),
                long_wish.replace(' ', "+")
            );
            links.push(join(&r, &invite, &form).await);
        }
        let page = get(&r, &admin).await;
        assert!(
            page.has("Guinevere Ottoline Beauchamp-Ravensworth<"),
            "names are cut to 40 characters"
        );
        assert!(
            !page.has("Extravaganza 2026 and more"),
            "group names are cut to 60 characters"
        );
        post(&r, &format!("{admin}/draw"), "").await;
        for link in &links {
            let shown = post(&r, &format!("{link}/open"), "").await.to();
            let res = get(&r, &shown).await;
            assert!(
                res.has("class=\"full\""),
                "long names show the full name small"
            );
            assert!(
                res.has("Show all wishes") && res.has("wish-body"),
                "long wishes collapse"
            );
            let full = between(
                &res.body,
                "<summary class=\"link\">Show all wishes</summary><p class=\"wish-body\">",
                "</p>",
            );
            assert_eq!(
                full.chars().count(),
                WISHES_MAX,
                "wishes are cut to 600 characters"
            );
        }
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn static_files() {
        let (r, path) = test_app();
        for (file, kind) in [
            ("style.css", "text/css"),
            ("app.js", "text/javascript"),
            ("town.svg", "image/svg+xml"),
            ("clouds.svg", "image/svg+xml"),
            ("fell.woff2", "font/woff2"),
            ("fell-italic.woff2", "font/woff2"),
        ] {
            let res = get(&r, &format!("/static/{file}")).await;
            assert_eq!(res.status, StatusCode::OK, "{file}");
            assert!(
                res.content_type.starts_with(kind),
                "{file}: {}",
                res.content_type
            );
        }
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn unknown_links_are_404() {
        let (r, path) = test_app();
        for uri in [
            "/g/nope",
            "/j/nope",
            "/p/nope",
            "/p/nope/state",
            "/static/nope",
            "/nope",
        ] {
            assert_eq!(get(&r, uri).await.status, StatusCode::NOT_FOUND, "{uri}");
        }
        assert_eq!(
            post(&r, "/g/nope/draw", "").await.status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            post(&r, "/p/nope/open", "").await.status,
            StatusCode::NOT_FOUND
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn helpers() {
        assert_eq!(initials("Bartholomew J. Winterbottom III"), "BW");
        assert_eq!(initials("Nana Jo"), "NJ");
        assert_eq!(initials("Jo"), "J");
        assert_eq!(day_month(0), "1 Jan");
        assert_eq!(day_month(1_798_761_600), "1 Jan"); // 2027-01-01
        assert_eq!(clean_line("  a \t b\u{0007} ", 40), "a b");
        assert_eq!(clean_text("a\r\nb  \r\n\r\n", 600), "a\nb");
        assert_eq!(token().len(), 26);
        assert!(invite_token().split('-').count() == 3);
    }
}
