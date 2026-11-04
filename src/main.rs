//! Kringle: a tiny Kris Kringle / Secret Santa gift swap server.
//!
//! One static binary, small enough for a home router (built for the
//! GL.iNet Flint 2). A host starts a group, shares one invite link, adds
//! "keep apart" rules and draws names. No accounts: three kinds of
//! unguessable link (admin, invite, personal). State lives in one JSON file
//! that is rewritten atomically after every change.
//!
//! Everything but email is in this file: config, storage, the draw, HTTP
//! handlers, and HTML (maud). Email (SMTP or Tuta) is in `mail.rs`. The CSS,
//! the little JavaScript, fonts and the two illustrations are compiled in
//! from `static/`.

mod mail;

use std::{
    collections::HashMap,
    convert::Infallible,
    fs,
    future::Future,
    io::{self, Write as _},
    net::IpAddr,
    os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use http_body_util::{BodyExt as _, Full, Limited};
use hyper::{
    Method, Request, Response, StatusCode,
    body::Bytes,
    header::{self, HeaderMap, HeaderValue},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::{TokioIo, TokioTimer};
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
/// Groups nobody but the host ever joined are removed after this long.
const EMPTY_GROUP_SECS: u64 = 7 * 86_400;
/// Each address can start this many groups at once, then one more every
/// `CREATE_EVERY_SECS`.
const CREATE_BURST: u32 = 5;
const CREATE_EVERY_SECS: u64 = 12 * 60;
/// Wishes longer than this show a preview on the gift tag, with the rest
/// behind "Show all wishes".
const WISHES_PREVIEW: usize = 240;
/// Names longer than this show the first name big and the full name small.
const TAG_NAME_FULL: usize = 14;

// ---------------------------------------------------------------------------
// Config and entry point
// ---------------------------------------------------------------------------

#[derive(PartialEq)]
enum Mode {
    Serve,
    /// Sign in to the mail server or Tuta once, report, and exit.
    CheckMail,
    /// Print email counts from the data file as JSON, and exit.
    MailStatus,
}

struct Config {
    mode: Mode,
    addr: String,
    data: PathBuf,
    public_url: Option<String>,
    keep_days: u64,
}

const USAGE: &str = "\
kringle: a tiny Kris Kringle / Secret Santa gift swap server

USAGE: kringle [--addr HOST:PORT] [--data FILE] [--public-url URL] [--keep-days N]
       kringle --check-mail              sign in with the email settings, then exit
       kringle --mail-status [--data FILE]
                                         email counts as JSON (sent, waiting, failed)

  --addr        where to listen            (env KRINGLE_ADDR, default 0.0.0.0:8787)
  --data        JSON file for all groups   (env KRINGLE_DATA, default ./kringle.json)
  --public-url  base for links people get, e.g. http://192.168.8.1:8787
                (env KRINGLE_PUBLIC_URL, default: the Host the browser used)
  --keep-days   delete groups this many days after they were created
                (env KRINGLE_KEEP_DAYS, default 120)
";

fn usage() -> String {
    format!("{USAGE}\n{}", mail::usage())
}

fn config() -> Config {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let mut cfg = Config {
        mode: Mode::Serve,
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
                eprintln!("{name} needs a value\n\n{}", usage());
                std::process::exit(2)
            })
        };
        match arg.as_str() {
            "--addr" => cfg.addr = value("--addr"),
            "--data" => cfg.data = value("--data").into(),
            "--public-url" => cfg.public_url = Some(value("--public-url")),
            "--check-mail" => cfg.mode = Mode::CheckMail,
            "--mail-status" => cfg.mode = Mode::MailStatus,
            "--keep-days" => {
                cfg.keep_days = value("--keep-days").parse().unwrap_or_else(|_| {
                    eprintln!("--keep-days needs a whole number");
                    std::process::exit(2)
                })
            }
            "-h" | "--help" => {
                print!("{}", usage());
                std::process::exit(0)
            }
            "-V" | "--version" => {
                println!("kringle {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0)
            }
            other => {
                eprintln!("unknown argument: {other}\n\n{}", usage());
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
    if cfg.mode == Mode::MailStatus {
        let store = Store::load(cfg.data.clone()).unwrap_or_else(|e| {
            eprintln!("kringle: can't read {}: {e}", cfg.data.display());
            std::process::exit(1)
        });
        println!("{}", mail_status(&store.data));
        return;
    }
    let mail = mail::Transport::from_env().unwrap_or_else(|e| {
        eprintln!("kringle: email settings: {e}");
        std::process::exit(2)
    });
    if cfg.mode == Mode::CheckMail {
        let Some(mail) = mail else {
            println!("Email is off.");
            return;
        };
        let checked = tokio::time::timeout(CHECK_TIMEOUT, mail.check())
            .await
            .unwrap_or_else(|_| Err("No answer within 20 seconds.".into()));
        match checked {
            Ok(ok) => println!("{ok}"),
            Err(why) => {
                println!("{why}");
                std::process::exit(1)
            }
        }
        return;
    }
    let mut store = Store::load(cfg.data.clone()).unwrap_or_else(|e| {
        eprintln!("kringle: can't read {}: {e}", cfg.data.display());
        std::process::exit(1)
    });
    let keep_secs = cfg.keep_days * 86_400;
    if store.prune(now(), keep_secs)
        && let Err(e) = store.save()
    {
        eprintln!("kringle: saving after removing old groups failed: {e}");
    }
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
    match &mail {
        Some(m) => eprintln!("kringle: email: {}", m.describe()),
        None => eprintln!("kringle: email is off (set KRINGLE_MAIL to turn it on)"),
    }
    let app = Arc::new(App {
        store: Mutex::new(store),
        public_url: cfg.public_url,
        keep_secs,
        limiter: Mutex::new(HashMap::new()),
        mail,
        mail_wake: tokio::sync::Notify::new(),
        outbox: Mutex::new(Vec::new()),
        lockout: Mutex::new(HashMap::new()),
    });
    tokio::spawn(housekeeping(app.clone()));
    tokio::spawn(mail_worker(app.clone()));
    serve(listener, app.clone(), shutdown()).await;
    if let Some(mail) = &app.mail {
        let _ = tokio::time::timeout(Duration::from_secs(3), mail.close()).await;
    }
}

/// `--check-mail` gives up after this (LuCI waits on it).
const CHECK_TIMEOUT: Duration = Duration::from_secs(20);

/// Email counts across every group, for the LuCI page: no names, no
/// addresses, no reasons.
fn mail_status(data: &Data) -> String {
    let (mut sent, mut waiting, mut failed, mut groups_with_failures) = (0, 0, 0, 0);
    for g in &data.groups {
        let mut group_failed = false;
        let host = (!g.host_email.is_empty()).then_some(&g.host_mail);
        for state in g.members.iter().map(|m| &m.mail).chain(host) {
            match state {
                MailState::None => {}
                MailState::Pending | MailState::Sending => waiting += 1,
                MailState::Sent => sent += 1,
                MailState::Failed(_) => {
                    failed += 1;
                    group_failed = true;
                }
            }
        }
        groups_with_failures += group_failed as u32;
    }
    serde_json::json!({
        "sent": sent,
        "waiting": waiting,
        "failed": failed,
        "groups_with_failures": groups_with_failures,
    })
    .to_string()
}

/// Remove expired groups every hour, even when nobody is using the server.
async fn housekeeping(app: Shared) {
    let mut hourly = tokio::time::interval(Duration::from_secs(3600));
    loop {
        hourly.tick().await;
        let mut st = lock(&app);
        if st.prune(now(), app.keep_secs)
            && let Err(e) = st.save()
        {
            eprintln!("kringle: saving after removing old groups failed: {e}");
        }
    }
}

/// Send queued emails, one at a time, whenever something is queued.
async fn mail_worker(app: Shared) {
    loop {
        deliver(&app).await;
        app.mail_wake.notified().await;
    }
}

/// Whom an email is for: a member, or a host who isn't in the draw.
#[derive(Clone, Copy)]
enum Recipient {
    Member(u32),
    Host,
}

/// Send every pending email, then return. A failure is logged and kept on
/// the person with its reason, for the host to see and resend.
async fn deliver(app: &App) {
    let Some(transport) = &app.mail else { return };
    // Links for people who are locked out: no group to record the result on,
    // so a failure is only logged.
    let letters = std::mem::take(&mut *app.outbox.lock().unwrap_or_else(|e| e.into_inner()));
    for letter in letters {
        if let Err(why) = transport.send(&letter).await {
            eprintln!("kringle: links email failed: {why}");
        }
    }
    loop {
        let job = {
            let mut st = lock(app);
            next_letter(&mut st.data, app.public_url.as_deref())
        };
        let Some((admin, who, letter)) = job else {
            return;
        };
        let result = transport.send(&letter).await;
        let mut st = lock(app);
        let Some(g) = st.by_admin(&admin) else {
            continue;
        };
        if let Err(why) = &result {
            eprintln!(
                "kringle: email to {} in “{}” failed: {why}",
                letter.to_name, g.name
            );
        }
        let state = match who {
            Recipient::Host => Some(&mut g.host_mail),
            Recipient::Member(id) => g
                .members
                .iter_mut()
                .find(|m| m.id == id)
                .map(|m| &mut m.mail),
        };
        // Queued again meanwhile (say names were drawn while the welcome
        // was going out): leave it queued so the newer email goes too.
        if let Some(state) = state
            && *state == MailState::Sending
        {
            *state = match result {
                Ok(()) => MailState::Sent,
                Err(why) => MailState::Failed(why),
            };
            if let Err(e) = st.save() {
                eprintln!("kringle: saving email status failed: {e}");
            }
        }
    }
}

/// The next queued email, marked as being sent: (group admin token, whom,
/// the letter).
fn next_letter(
    data: &mut Data,
    public_url: Option<&str>,
) -> Option<(String, Recipient, mail::Letter)> {
    for g in &mut data.groups {
        let base = public_url.unwrap_or(&g.base).to_string();
        let queued = |s: &MailState| matches!(s, MailState::Pending | MailState::Sending);
        let who = if queued(&g.host_mail) && !g.host_email.is_empty() {
            Recipient::Host
        } else if let Some(m) = g
            .members
            .iter()
            .find(|m| queued(&m.mail) && !m.email.is_empty())
        {
            Recipient::Member(m.id)
        } else {
            continue;
        };
        let letter = letter_for(g, who, &base);
        match who {
            Recipient::Host => g.host_mail = MailState::Sending,
            Recipient::Member(id) => {
                if let Some(m) = g.members.iter_mut().find(|m| m.id == id) {
                    m.mail = MailState::Sending;
                }
            }
        }
        return Some((g.admin.clone(), who, letter));
    }
    None
}

/// The email someone gets now: the host's welcome with the admin link, a
/// guest's welcome with their personal link, or, once names are drawn, the
/// link to unwrap their gift tag.
fn letter_for(g: &Group, who: Recipient, base: &str) -> mail::Letter {
    let admin_url = format!("{base}/g/{}", g.admin);
    let invite_url = format!("{base}/j/{}", g.invite);
    let budget = if g.budget.is_empty() {
        String::new()
    } else {
        format!(" The budget is ${}.", g.budget)
    };
    let (name, email, personal, is_host, drawn) = match who {
        Recipient::Host => (g.host.as_str(), g.host_email.as_str(), None, true, false),
        Recipient::Member(id) => {
            let m = g.member(id).expect("member exists");
            let url = format!("{base}/p/{}", m.token);
            (
                m.name.as_str(),
                m.email.as_str(),
                Some(url),
                m.host,
                g.drawn.is_some() && m.gives_to.is_some(),
            )
        }
    };
    let hi = format!("Hi {},", first_word(name));
    let (subject, text) = if drawn {
        let url = personal.unwrap_or_default();
        (
            format!("Names are drawn for {}", g.name),
            format!(
                "{hi}\n\n{} has drawn names for {}. Unwrap your gift tag to see who you’re giving to:\n\n{url}\n\nOnly you can see your tag.{budget}\n\nKringle",
                first_word(&g.host),
                g.name
            ),
        )
    } else if is_host {
        let own = match personal {
            Some(url) => {
                format!("Your own page, for your wishes and later your gift tag:\n{url}\n\n")
            }
            None => String::new(),
        };
        (
            format!("Your gift swap {} is ready", g.name),
            format!(
                "{hi}\n\nYour group {} is set up.\n\nYour admin link. Keep it private: it’s the only key to the group.\n{admin_url}\n\n{own}Send this invite link to everyone taking part:\n{invite_url}\n\nKringle",
                g.name
            ),
        )
    } else {
        let url = personal.unwrap_or_default();
        (
            format!("You’re in {}", g.name),
            format!(
                "{hi}\n\nYou’ve joined {}’s gift swap, {}. This is your personal link. Your gift tag appears there once names are drawn, and we’ll email you when that happens.\n\n{url}\n\nKeep it to yourself: it’s how you get back in.{budget}\n\nKringle",
                first_word(&g.host),
                g.name
            ),
        )
    };
    mail::Letter {
        to_name: name.to_string(),
        to: email.to_string(),
        subject,
        text,
    }
}

/// Accept connections until `shutdown` resolves, one lightweight task per
/// connection. Saves happen synchronously under the store lock, so stopping
/// never leaves a half-written change behind.
async fn serve(listener: tokio::net::TcpListener, app: Shared, shutdown: impl Future<Output = ()>) {
    tokio::pin!(shutdown);
    // At most MAX_CONNECTIONS at once; beyond that, new ones wait in the
    // kernel's queue instead of using up memory and file descriptors.
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        let slot = tokio::select! {
            slot = slots.clone().acquire_owned() => slot.expect("semaphore never closed"),
            _ = &mut shutdown => return,
        };
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(conn) => conn,
                Err(e) => {
                    eprintln!("kringle: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
            _ = &mut shutdown => return,
        };
        let app = app.clone();
        tokio::spawn(async move {
            let _slot = slot;
            let peer = Some(peer.ip());
            let service = service_fn(move |req| {
                let app = app.clone();
                async move { Ok::<_, Infallible>(handle(&app, peer, req).await) }
            });
            let _ = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_TIMEOUT)
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
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

#[derive(Serialize, Deserialize, Clone, Default)]
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
    /// Everyone must give an email to join. Groups from before emails
    /// existed load with this off.
    #[serde(default)]
    require_email: bool,
    /// The host's email when they aren't in the draw (otherwise it's on
    /// their member entry), and how sending to it went.
    #[serde(default)]
    host_email: String,
    #[serde(default)]
    host_mail: MailState,
    /// The address the group was created through, for links in emails when
    /// no public address is configured.
    #[serde(default)]
    base: String,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct Member {
    id: u32,
    name: String,
    wishes: String,
    token: String,
    host: bool,
    opened: bool,
    gives_to: Option<u32>,
    #[serde(default)]
    email: String,
    #[serde(default)]
    mail: MailState,
}

/// Where the latest email to someone is. `Pending` is the queue: the mail
/// worker sends whatever is pending, so a restart picks up where it left off.
/// Which email it is (welcome or "names are drawn") is decided when sending.
#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
enum MailState {
    #[default]
    None,
    Pending,
    /// Picked up by the worker (in memory only; a stale one is retried).
    Sending,
    Sent,
    Failed(String),
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
            // Private: the file holds every group's secret links and the draw.
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.set_permissions(fs::Permissions::from_mode(0o600))?;
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

    /// Remove groups older than `keep_secs`, and groups nobody but the host
    /// joined within `EMPTY_GROUP_SECS`. Returns whether anything went.
    fn prune(&mut self, now: u64, keep_secs: u64) -> bool {
        let before = self.data.groups.len();
        self.data.groups.retain(|g| {
            let age = now.saturating_sub(g.created);
            age < keep_secs && (age < EMPTY_GROUP_SECS || g.members.iter().any(|m| !m.host))
        });
        self.data.groups.len() != before
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

/// Saints and a fitting group name, for the grey example text in forms:
/// nobody typing is called "St. Nicholas", so it can't be mistaken for
/// something already filled in.
const EXAMPLES: [(&str, &str); 20] = [
    ("St. Nicholas", "The Stocking Fillers"),
    ("St. Stephen", "Stephen’s Boxing Day Bunch"),
    ("St. Lucy", "The Lucy Light Brigade"),
    ("St. Francis", "Francis & Friends of the Forest"),
    ("St. Agnes", "The Lambs of Agnes"),
    ("St. Cecilia", "The Cecilia Carol Club"),
    ("St. Benedict", "Benedict’s Bread & Brandy"),
    ("St. Ambrose", "The Ambrose Bee Keepers"),
    ("St. Bridget", "Bridget’s Brew Crew"),
    ("St. Columba", "The Columba Doves"),
    ("St. Hilda", "The Whitby Abbey Exchange"),
    ("St. Swithin", "Swithin’s Rainy Day Fund"),
    ("St. Alban", "Alban’s Last Stand"),
    ("St. Cuthbert", "Cuthbert’s Lindisfarne Lot"),
    ("St. Aidan", "Aidan’s Beacon Fire"),
    ("St. Anselm", "Anselm’s Big Argument"),
    ("St. Dunstan", "Dunstan’s Hot Tongs Society"),
    ("St. Winifred", "The Winifred Wishing Well"),
    ("St. Crispin", "The Crispin Cobblers"),
    ("St. Martin", "Martin’s Half-Cloak Club"),
];

/// A random example (name, group name, email), chosen per page load.
fn example() -> (&'static str, &'static str, String) {
    let (name, group) = EXAMPLES[rand_below(EXAMPLES.len())];
    let first = name.strip_prefix("St. ").unwrap_or(name);
    let email = format!("{}@example.com", first.to_lowercase());
    (name, group, email)
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

/// Largest form we accept.
const BODY_MAX: usize = 16 * 1024;
/// How long a client gets to send its request headers, and then its form.
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const BODY_TIMEOUT: Duration = Duration::from_secs(10);
/// Open connections served at once.
const MAX_CONNECTIONS: usize = 128;

struct App {
    store: Mutex<Store>,
    public_url: Option<String>,
    keep_secs: u64,
    /// Per-address allowance for starting groups: (groups left, last refill).
    limiter: Mutex<HashMap<IpAddr, (u32, u64)>>,
    /// How to send email; `None` when email is off.
    mail: Option<mail::Transport>,
    /// Wakes the mail worker after something was queued.
    mail_wake: tokio::sync::Notify,
    /// Emails not tied to a group, waiting for the mail worker (the
    /// "Looking for a swap?" link emails).
    outbox: Mutex<Vec<mail::Letter>>,
    /// When each address, and each peer, last asked for its links.
    lockout: Mutex<HashMap<String, u64>>,
}

impl App {
    /// Whether `peer` may start another group now. Unknown peers (tests) may.
    fn may_create(&self, peer: Option<IpAddr>, now: u64) -> bool {
        let Some(ip) = peer else { return true };
        let mut buckets = self.limiter.lock().unwrap_or_else(|e| e.into_inner());
        if buckets.len() > 10_000 {
            // Forget addresses that are back to a full allowance.
            buckets.retain(|_, (_, last)| {
                now.saturating_sub(*last) < CREATE_EVERY_SECS * CREATE_BURST as u64
            });
        }
        let (left, last) = buckets.entry(ip).or_insert((CREATE_BURST, now));
        let earned = now.saturating_sub(*last) / CREATE_EVERY_SECS;
        if earned > 0 {
            *left = (*left).saturating_add(earned as u32).min(CREATE_BURST);
            *last += earned * CREATE_EVERY_SECS;
        }
        if *left == 0 {
            return false;
        }
        *left -= 1;
        true
    }
}

type Shared = Arc<App>;
type Res = Response<Full<Bytes>>;

fn lock(app: &App) -> MutexGuard<'_, Store> {
    app.store.lock().unwrap_or_else(|e| e.into_inner())
}

/// Route a request. Paths are split on `/`; a known path with the wrong method
/// gets 405, anything else 404. Every response gets the security headers.
async fn handle<B>(app: &App, peer: Option<IpAddr>, req: Request<B>) -> Res
where
    B: hyper::body::Body<Data = Bytes>,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let (parts, body) = req.into_parts();
    let (get, post) = (parts.method == Method::GET, parts.method == Method::POST);
    let (h, query) = (&parts.headers, parts.uri.query().unwrap_or(""));
    let segs: Vec<&str> = parts.uri.path().split('/').skip(1).collect();
    let mut res = match segs.as_slice() {
        [""] if get => index(app),
        ["groups"] if post => match read_form(body).await {
            Ok(f) => create(app, peer, &f, h),
            Err(res) => *res,
        },
        ["g", t] if get => admin(app, t, query, h),
        ["g", t, "rules"] if post => match read_form(body).await {
            Ok(f) => add_rule(app, t, &f),
            Err(res) => *res,
        },
        ["g", t, "rules", i, "delete"] if post => match i.parse() {
            Ok(i) => remove_rule(app, t, i),
            Err(_) => not_found(),
        },
        ["g", t, "people", id, "delete"] if post => match id.parse() {
            Ok(id) => remove_person(app, t, id),
            Err(_) => not_found(),
        },
        ["g", t, "draw"] if post => draw_names(app, t),
        ["g", t, "undraw"] if post => undraw(app, t),
        ["g", t, "resend"] if post => resend(app, t),
        ["g", t, "delete"] if post => delete_group(app, t),
        ["deleted"] if get => page(deleted_page()),
        ["locked-out"] if get => locked_out(app, None, ""),
        ["locked-out"] if post => match read_form(body).await {
            Ok(f) => locked_out_post(app, peer, &f),
            Err(res) => *res,
        },
        ["j", t] if get => join(app, t, h),
        ["j", t] if post => match read_form(body).await {
            Ok(f) => join_post(app, t, &f),
            Err(res) => *res,
        },
        ["p", t] if get => me(app, t, query, h),
        ["p", t, "wishes"] if post => match read_form(body).await {
            Ok(f) => save_wishes(app, t, &f),
            Err(res) => *res,
        },
        ["p", t, "open"] if post => unwrap_tag(app, t),
        ["p", t, "state"] if get => draw_state(app, t),
        ["static", file] if get => asset(file),
        known if is_route(known) => status(StatusCode::METHOD_NOT_ALLOWED),
        _ => not_found(),
    };
    security_headers(&mut res);
    res
}

fn is_route(segs: &[&str]) -> bool {
    matches!(
        segs,
        [""] | ["groups"]
            | ["g", _]
            | ["deleted"]
            | ["locked-out"]
            | ["g", _, "rules" | "draw" | "undraw" | "delete" | "resend"]
            | ["g", _, "rules" | "people", _, "delete"]
            | ["j", _]
            | ["p", _]
            | ["p", _, "wishes" | "open" | "state"]
            | ["static", _]
    )
}

/// A submitted form: name/value pairs, first value wins.
struct Form(Vec<(String, String)>);

impl Form {
    fn get(&self, key: &str) -> String {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    fn has(&self, key: &str) -> bool {
        self.0.iter().any(|(k, _)| k == key)
    }
}

/// Read an urlencoded form body: at most `BODY_MAX` bytes, arriving within
/// `BODY_TIMEOUT`. Otherwise the response to send instead.
async fn read_form<B>(body: B) -> Result<Form, Box<Res>>
where
    B: hyper::body::Body<Data = Bytes>,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    match tokio::time::timeout(BODY_TIMEOUT, Limited::new(body, BODY_MAX).collect()).await {
        Ok(Ok(collected)) => Ok(Form(
            form_urlencoded::parse(&collected.to_bytes())
                .into_owned()
                .collect(),
        )),
        Ok(Err(e)) if e.is::<http_body_util::LengthLimitError>() => Err(Box::new(too_large())),
        Ok(Err(_)) => Err(Box::new(status(StatusCode::BAD_REQUEST))),
        Err(_) => Err(Box::new(respond(
            StatusCode::REQUEST_TIMEOUT,
            "text/plain; charset=utf-8",
            "That took too long to arrive.",
        ))),
    }
}

fn query_param(query: &str, key: &str) -> Option<String> {
    form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

fn security_headers(res: &mut Res) {
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

fn respond(code: StatusCode, content_type: &'static str, body: impl Into<Bytes>) -> Res {
    let mut res = Response::new(Full::new(body.into()));
    *res.status_mut() = code;
    res.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    res
}

fn status(code: StatusCode) -> Res {
    let mut res = Response::new(Full::new(Bytes::new()));
    *res.status_mut() = code;
    res
}

fn page(markup: Markup) -> Res {
    respond(
        StatusCode::OK,
        "text/html; charset=utf-8",
        markup.into_string(),
    )
}

/// 303 See Other: after a form post, the browser GETs `path`.
fn to(path: String) -> Res {
    let mut res = status(StatusCode::SEE_OTHER);
    if let Ok(v) = HeaderValue::try_from(path) {
        res.headers_mut().insert(header::LOCATION, v);
    }
    res
}

fn too_large() -> Res {
    respond(
        StatusCode::PAYLOAD_TOO_LARGE,
        "text/plain; charset=utf-8",
        "That form is too big.",
    )
}

fn not_found() -> Res {
    let body = layout(
        "Link not found · Kringle",
        html! {},
        html! {
            section.panel {
                h1 { "This link doesn’t work" }
                p { "It may have a typo, or the group may have been deleted. Ask whoever sent it to check." }
                a.btn href="/" { (icon(GIFT_ICON)) "Start a new gift swap" }
            }
        },
    );
    respond(
        StatusCode::NOT_FOUND,
        "text/html; charset=utf-8",
        body.into_string(),
    )
}

fn save_failed(e: io::Error) -> Res {
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
    respond(
        StatusCode::INTERNAL_SERVER_ERROR,
        "text/html; charset=utf-8",
        body.into_string(),
    )
}

/// The stylesheet, script and illustrations are embedded gzipped by build.rs,
/// which keeps the binary small and pages fast. Every browser accepts gzip,
/// so they are always sent that way.
macro_rules! gz {
    ($file:literal) => {
        include_bytes!(concat!(env!("OUT_DIR"), "/", $file, ".gz"))
    };
}

/// A short hash of the stylesheet and script, put on their URLs so a browser
/// that cached an old copy (they're kept for a day) fetches the new one as
/// soon as the pages that need it are served.
fn asset_version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| {
        // FNV-1a: no dependency needed for a cache key.
        let mut h: u64 = 0xcbf29ce484222325;
        for b in gz!("style.css").iter().chain(gz!("app.js").iter()) {
            h = (h ^ *b as u64).wrapping_mul(0x100000001b3);
        }
        format!("{:08x}", h >> 32)
    })
}

fn asset(file: &str) -> Res {
    let (kind, gzipped, body): (&'static str, bool, &'static [u8]) = match file {
        "style.css" => ("text/css; charset=utf-8", true, gz!("style.css")),
        "app.js" => ("text/javascript; charset=utf-8", true, gz!("app.js")),
        "town.svg" => ("image/svg+xml", true, gz!("town.svg")),
        "clouds.svg" => ("image/svg+xml", true, gz!("clouds.svg")),
        "fell.woff2" => (
            "font/woff2",
            false,
            include_bytes!("../static/fonts/fell.woff2"),
        ),
        "fell-italic.woff2" => (
            "font/woff2",
            false,
            include_bytes!("../static/fonts/fell-italic.woff2"),
        ),
        _ => return not_found(),
    };
    let mut res = respond(StatusCode::OK, kind, Bytes::from_static(body));
    let h = res.headers_mut();
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=86400"),
    );
    if gzipped {
        h.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    }
    res
}

// ---------------------------------------------------------------------------
// Handlers: start a group
// ---------------------------------------------------------------------------

#[derive(Default)]
struct CreateForm {
    group: String,
    host: String,
    budget: String,
    plays: bool,
    email: String,
    require_email: bool,
}

fn index(app: &App) -> Res {
    page(create_page(
        &CreateForm {
            plays: true,
            require_email: true,
            ..Default::default()
        },
        None,
        app.mail.is_some(),
    ))
}

/// The email typed into a form: `Ok("")` when left empty and not required,
/// else the cleaned address or the message to show.
fn form_email(typed: &str, required: bool) -> Result<String, &'static str> {
    if typed.trim().is_empty() {
        return if required {
            Err("Add your email. This group sends everyone their link by email.")
        } else {
            Ok(String::new())
        };
    }
    mail::clean_address(typed).ok_or("That email address doesn’t look right.")
}

fn create(app: &App, peer: Option<IpAddr>, form: &Form, headers: &HeaderMap) -> Res {
    let mail_on = app.mail.is_some();
    let f = CreateForm {
        group: form.get("group"),
        host: form.get("host"),
        budget: form.get("budget"),
        plays: form.has("plays"),
        email: form.get("email"),
        // Without a way to send email there's nothing to require.
        require_email: mail_on && form.has("require_email"),
    };
    let create_page = |f: &CreateForm, e: Option<&str>| create_page(f, e, mail_on);
    // "Use emails" off hides the host's email box, so whatever it held is
    // ignored.
    let email = if mail_on && f.require_email {
        form_email(&f.email, true)
    } else {
        Ok(String::new())
    };
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
        email.as_ref().err().copied()
    };
    let email = email.unwrap_or_default();
    let mail = if email.is_empty() {
        MailState::None
    } else {
        MailState::Pending
    };
    if let Some(e) = error {
        return page(create_page(&f, Some(e)));
    }
    if !app.may_create(peer, now()) {
        let mut res = page(create_page(
            &f,
            Some("You’ve started several groups in a short time. Try again in a little while."),
        ));
        *res.status_mut() = StatusCode::TOO_MANY_REQUESTS;
        return res;
    }
    let mut st = lock(app);
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
        require_email: f.require_email,
        base: base_url(app, headers),
        ..Default::default()
    };
    if f.plays {
        g.members.push(Member {
            id: 1,
            name: host,
            token: token(),
            host: true,
            email,
            mail,
            ..Default::default()
        });
        g.next_id = 2;
    } else {
        g.host_email = email;
        g.host_mail = mail;
    }
    let admin = g.admin.clone();
    st.data.groups.push(g);
    if let Err(e) = st.save() {
        st.data.groups.pop();
        return save_failed(e);
    }
    drop(st);
    app.mail_wake.notify_one();
    to(format!("/g/{admin}"))
}

// ---------------------------------------------------------------------------
// Handlers: host dashboard
// ---------------------------------------------------------------------------

fn admin(app: &App, t: &str, query: &str, headers: &HeaderMap) -> Res {
    let base = base_url(app, headers);
    let mut st = lock(app);
    let Some(g) = st.by_admin(t) else {
        return not_found();
    };
    let error = match query_param(query, "err").as_deref() {
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
) -> Res {
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

fn add_rule(app: &App, t: &str, f: &Form) -> Res {
    admin_change(app, t, "rules", |g| {
        if g.drawn.is_some() {
            return Some("drawn");
        }
        let (Ok(giver), Ok(receiver)) = (
            f.get("giver").parse::<u32>(),
            f.get("receiver").parse::<u32>(),
        ) else {
            return Some("same");
        };
        if giver == receiver || g.index_of(giver).is_none() || g.index_of(receiver).is_none() {
            return Some("same");
        }
        let both = f.has("both");
        // An existing rule between the same two people either already covers
        // this one, or gets upgraded to both ways (e.g. B -> A, then A <-> B).
        let same_pair = |r: &Rule| {
            (r.giver == giver && r.receiver == receiver)
                || (r.giver == receiver && r.receiver == giver)
        };
        if let Some(r) = g.rules.iter_mut().find(|r| same_pair(r)) {
            let covered = r.both || (!both && r.giver == giver);
            if covered {
                return Some("dup");
            }
            r.both = true;
            return None;
        }
        g.rules.push(Rule {
            giver,
            receiver,
            both,
        });
        None
    })
}

fn remove_rule(app: &App, t: &str, i: usize) -> Res {
    admin_change(app, t, "rules", |g| {
        if g.drawn.is_some() {
            return Some("drawn");
        }
        if i < g.rules.len() {
            g.rules.remove(i);
        }
        None
    })
}

fn remove_person(app: &App, t: &str, id: u32) -> Res {
    admin_change(app, t, "people", |g| {
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

fn draw_names(app: &App, t: &str) -> Res {
    let mail_on = app.mail.is_some();
    let res = admin_change(app, t, "draw", |g| {
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
            if mail_on && !m.email.is_empty() {
                m.mail = MailState::Pending;
            }
        }
        g.drawn = Some(now());
        None
    });
    app.mail_wake.notify_one();
    res
}

/// Try every failed email again.
fn resend(app: &App, t: &str) -> Res {
    let res = admin_change(app, t, "mail", |g| {
        let states = g
            .members
            .iter_mut()
            .map(|m| &mut m.mail)
            .chain([&mut g.host_mail]);
        for state in states {
            if matches!(state, MailState::Failed(_)) {
                *state = MailState::Pending;
            }
        }
        None
    });
    app.mail_wake.notify_one();
    res
}

fn delete_group(app: &App, t: &str) -> Res {
    let mut st = lock(app);
    let Some(i) = st.data.groups.iter().position(|g| g.admin == t) else {
        return not_found();
    };
    let gone = st.data.groups.remove(i);
    if let Err(e) = st.save() {
        st.data.groups.insert(i, gone);
        return save_failed(e);
    }
    to("/deleted".into())
}

fn undraw(app: &App, t: &str) -> Res {
    admin_change(app, t, "top", |g| {
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

#[derive(Default)]
struct JoinForm {
    name: String,
    wishes: String,
    email: String,
}

fn join(app: &App, t: &str, headers: &HeaderMap) -> Res {
    let mut st = lock(app);
    let Some(g) = st.by_invite(t) else {
        return not_found();
    };
    let returning =
        cookie(headers, "kringle").and_then(|c| g.members.iter().find(|m| m.token == c));
    let ask_email = app.mail.is_some() || g.require_email;
    page(join_page(
        g,
        returning,
        &JoinForm::default(),
        None,
        ask_email,
    ))
}

fn join_post(app: &App, t: &str, form: &Form) -> Res {
    let f = JoinForm {
        name: form.get("name"),
        wishes: form.get("wishes"),
        email: form.get("email"),
    };
    let mut st = lock(app);
    let Some(g) = st.by_invite(t) else {
        return not_found();
    };
    let ask_email = app.mail.is_some() || g.require_email;
    let name = clean_line(&f.name, NAME_MAX);
    let wishes = clean_text(&f.wishes, WISHES_MAX);
    let email = if ask_email {
        form_email(&f.email, g.require_email)
    } else {
        Ok(String::new())
    };
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
        email.as_ref().err().map(|e| e.to_string())
    };
    if let Some(e) = error {
        return page(join_page(g, None, &f, Some(&e), ask_email));
    }
    let email = email.unwrap_or_default();
    let mail = if email.is_empty() || app.mail.is_none() {
        MailState::None
    } else {
        MailState::Pending
    };
    let token = token();
    g.members.push(Member {
        id: g.next_id,
        name,
        wishes,
        token: token.clone(),
        email,
        mail,
        ..Default::default()
    });
    g.next_id += 1;
    if let Err(e) = st.save() {
        if let Some(g) = st.by_invite(t) {
            g.members.pop();
        }
        return save_failed(e);
    }
    drop(st);
    app.mail_wake.notify_one();
    // Lets them get back in by reopening the invite link on the same phone.
    let mut res = to(format!("/p/{token}"));
    let cookie = format!("kringle={token}; Path=/j/{t}; Max-Age=15552000; HttpOnly; SameSite=Lax");
    if let Ok(v) = HeaderValue::try_from(cookie) {
        res.headers_mut().insert(header::SET_COOKIE, v);
    }
    res
}

// ---------------------------------------------------------------------------
// Handlers: personal page and gift tag
// ---------------------------------------------------------------------------

fn me(app: &App, t: &str, query: &str, headers: &HeaderMap) -> Res {
    let base = base_url(app, headers);
    let mut st = lock(app);
    let Some((g, i)) = st.by_personal(t) else {
        return not_found();
    };
    let m = &g.members[i];
    if g.drawn.is_some()
        && let Some(to) = m.gives_to.and_then(|id| g.member(id))
    {
        let show = query_param(query, "show").is_some();
        return page(tag_page(g, m, to, show && m.opened));
    }
    page(waiting_page(g, m, &base))
}

fn save_wishes(app: &App, t: &str, f: &Form) -> Res {
    let mut st = lock(app);
    let Some((g, i)) = st.by_personal(t) else {
        return not_found();
    };
    let before = std::mem::replace(
        &mut g.members[i].wishes,
        clean_text(&f.get("wishes"), WISHES_MAX),
    );
    if let Err(e) = st.save() {
        if let Some((g, i)) = st.by_personal(t) {
            g.members[i].wishes = before;
        }
        return save_failed(e);
    }
    to(if f.get("back") == "tag" {
        format!("/p/{t}?show=1#tag")
    } else {
        format!("/p/{t}#wishes")
    })
}

fn unwrap_tag(app: &App, t: &str) -> Res {
    let mut st = lock(app);
    let Some((g, i)) = st.by_personal(t) else {
        return not_found();
    };
    if g.drawn.is_none() {
        return to(format!("/p/{t}"));
    }
    if !g.members[i].opened {
        g.members[i].opened = true;
        if let Err(e) = st.save() {
            if let Some((g, i)) = st.by_personal(t) {
                g.members[i].opened = false;
            }
            return save_failed(e);
        }
    }
    to(format!("/p/{t}?show=1#tag"))
}

/// Polled by the waiting page: 200 once names are drawn, else 204.
fn draw_state(app: &App, t: &str) -> Res {
    let mut st = lock(app);
    match st.by_personal(t) {
        Some((g, _)) if g.drawn.is_some() => status(StatusCode::OK),
        Some(_) => status(StatusCode::NO_CONTENT),
        None => status(StatusCode::NOT_FOUND),
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
                link rel="stylesheet" href=(format!("/static/style.css?v={}", asset_version()));
                script src=(format!("/static/app.js?v={}", asset_version())) defer {}
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

/// The red asterisk after the label of a required input. The stylesheet
/// hides it while the input is disabled.
fn star() -> Markup {
    html! { span.star aria-hidden="true" { "*" } }
}

fn icon(svg: &'static str) -> Markup {
    PreEscaped(svg.to_string())
}

fn create_page(f: &CreateForm, error: Option<&str>, mail_on: bool) -> Markup {
    let (eg_name, eg_group, eg_email) = example();
    layout(
        "Start a gift swap · Kringle",
        html! {
            @if mail_on {
                a.badge.red href="/locked-out" { (icon(SEARCH)) "Looking for a swap?" }
            } @else {
                span.tagline { "No sign-up. No app. Just a link." }
            }
        },
        html! {
            form.panel.stack method="post" action="/groups" {
                div.row {
                    h1 { "Start a gift swap" }
                    span.aside { "takes 1 min" }
                }
                @if mail_on {
                    p.small.under { a href="/locked-out" { "Looking for an existing swap?" } }
                }
                @if let Some(e) = error { p.error role="alert" { (e) } }
                label.field {
                    span { "Group name" (star()) }
                    input type="text" name="group" maxlength=(GROUP_MAX) required placeholder=(eg_group) value=(f.group);
                }
                div.field {
                    div.fieldhead {
                        label for="host" { "Your name" (star()) }
                        label.check.plain.inline {
                            input type="checkbox" name="plays" value="1" checked[f.plays];
                            "I’m in the draw too"
                        }
                    }
                    input #host type="text" name="host" maxlength=(NAME_MAX) required placeholder=(eg_name) autocomplete="given-name" value=(f.host);
                    span.hint { "Shown to everyone as the host." }
                }
                label.field {
                    span.row.start { "Budget" span.hint.opt { "(optional)" } }
                    span.money {
                        span aria-hidden="true" { "$" }
                        input type="text" name="budget" inputmode="numeric" maxlength=(BUDGET_MAX) placeholder="30" value=(f.budget);
                    }
                }
                @if mail_on {
                    div.emailbox {
                        label.check.plain {
                            input type="checkbox" name="require_email" value="1" checked[f.require_email];
                            "Use emails"
                        }
                        p.muted.small { "Everyone who joins gives an email and gets their link by email, and again when names are drawn." }
                        label.field {
                            span { "Your email" (star()) }
                            input type="email" name="email" maxlength="254" required placeholder=(eg_email) autocomplete="email" value=(f.email) data-enable-with="require_email" disabled[!f.require_email];
                            span.hint { "We email you your admin link." }
                        }
                    }
                }
                button.btn.primary.big type="submit" { (icon(GIFT_ICON)) "Create group" }
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
    let too_few = matches!(verdict, draw::Verdict::TooFew);
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
        "Draw settings (optional)".to_string()
    } else {
        format!("Draw settings · {}", plural(g.rules.len(), "rule", "rules"))
    };
    let opened = g.members.iter().filter(|m| m.opened).count();
    let my_page = g.host_member().map(|m| format!("/p/{}", m.token));

    // The host's own page: beside the invite link before the draw, under the
    // title after it.
    let own_page = html! {
        @if let Some(url) = &my_page {
            a.btn.green.small href=(url) { "Your own page" (icon(ARROW_RIGHT)) }
        }
    };

    layout(
        &format!("{} · Host · Kringle", g.name),
        html! {
            span.badge.host { (icon(LOCK)) "Host" span.hide-small { " view" } }
            button.btn.small.danger type="button" command="show-modal" commandfor="delete-dialog" { (icon(TRASH)) "Delete group" }
        },
        html! {
            div.intro.tight {
                h1.onsky { (g.name) }
                p.onsky-muted {
                    "Hosted by " (g.host)
                    @if !g.budget.is_empty() { " · $" (g.budget) " budget" }
                }
                @if g.drawn.is_some() { (own_page) }
            }
            @if let Some(e) = error { p.error role="alert" { (e) } }

            @if let Some(at) = g.drawn {
                section.drawn-card #draw {
                    span.kicker { "Drawn " (day_month(at)) }
                    h2 { "Gift tags are out" }
                    p { strong { (opened) " of " (n) " have opened their tag" } }
                    div.bar role="img" aria-label=(format!("{opened} of {n} opened")) {
                        span style=(format!("width: {}%", (opened * 100).checked_div(n).unwrap_or(0))) {}
                    }
                    p.small { "You see who’s opened theirs, never who they got." }
                }
                section.panel {
                    details.disclose {
                        summary { "Who’s opened & more options" }
                        ul.rows {
                            @for m in &g.members {
                                li {
                                    span.name title=(m.name) { (m.name) @if m.host { " (you)" } }
                                    @if m.opened { span.pill.yes { "Opened" } } @else { span.pill { "Not yet" } }
                                }
                            }
                        }
                        div.inset {
                            p { strong { "Someone lost their link?" } " They reopen the invite link on the same phone, or ask for it on the “Looking for a swap?” page." }
                            p { strong { "Need to change the group?" } " Cancelling reopens joining. When you draw again, everyone gets a new tag." }
                            form method="post" action=(format!("{admin_path}/undraw")) data-confirm="Cancel the draw and reopen joining? Everyone’s gift tag disappears until you draw again." {
                                button.btn type="submit" { (icon(UNDO)) "Cancel draw & reopen" }
                            }
                        }
                    }
                }
            } @else {
                section.hero #invite {
                    div.row {
                        h2 { "Invite people" }
                        (own_page)
                    }
                    p { "Send this link to the group chat." }
                    div.linkrow {
                        input type="text" readonly value=(invite_url) aria-label="Invite link";
                        button.btn type="button" data-share=(invite_url) data-share-title=(g.name) data-share-text=(format!("{} invited you to a gift swap, {}.", g.host, g.name)) { (icon(SHARE)) span { "Share" } }
                    }
                    details {
                        summary.link { (icon(QR)) "Show QR code" }
                        div.qr { (PreEscaped(qr_svg(&invite_url))) }
                    }
                }

                section.panel.tight #people data-poll=(admin_path) data-poll-ids="count chips manage-list" {
                    div.row {
                        h2 { "Who’s in " span.count #count { "· " (n) } }
                        button.btn.small type="button" command="show-modal" commandfor="people-dialog" { (icon(USERS)) "Manage" }
                    }
                    ul.chips #chips {
                        @for m in &g.members { li title=(m.name) { (m.name) @if m.host { " (you)" } } }
                        @if g.members.is_empty() { li.empty { "Nobody yet" } }
                    }
                }
                dialog #people-dialog aria-labelledby="people-title" {
                    h2 #people-title { "Remove people" }
                    ul.rows #manage-list {
                        @for m in g.members.iter().filter(|m| !m.host) {
                            li {
                                span.name title=(m.name) { (m.name) }
                                form method="post" action=(format!("{admin_path}/people/{}/delete", m.id)) data-confirm=(format!("Remove {} from the group?", m.name)) {
                                    button.btn.small type="submit" aria-label=(format!("Remove {}", m.name)) { (icon(TRASH)) "Remove" }
                                }
                            }
                        }
                        @if !g.members.iter().any(|m| !m.host) { li.empty { span.muted { "Nobody to remove yet." } } }
                    }
                    form method="dialog" { button.btn.small type="submit" { (icon(CROSS)) "Close" } }
                }

                @if too_few {
                    @if let Some((title, text)) = &problem {
                        div.problem.loud #draw role="alert" {
                            (icon(WARN))
                            span { strong { (title) } (text) }
                        }
                    }
                } @else {
                section.panel.tight #draw {
                    h2 { "Draw names" }
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
                            button.btn.primary.big type="submit" disabled[!ready] { (icon(SHUFFLE)) "Draw names" }
                        }
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
                                            button.btn type="submit" { (icon(PLUS)) "Add rule" }
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
                }
                }
            }

            div.strip {
                (icon(LOCK))
                p { strong { "This page is your admin link." } " Keep it private." }
                button.btn.small type="button" data-copy=(admin_url) { (icon(COPY)) span { "Copy admin link" } }
            }
            (mail_strip(g, &admin_path))

            dialog #delete-dialog aria-labelledby="delete-title" {
                h2 #delete-title { "Delete this group" }
                p { "This removes the group, everyone’s wishes and the draw for good. All of its links stop working." }
                div.actions {
                    form method="dialog" { button.btn type="submit" { (icon(BACK)) "Keep it" } }
                    form method="post" action=(format!("{admin_path}/delete")) {
                        button.btn.danger type="submit" { (icon(TRASH)) "Delete group" }
                    }
                }
            }
        },
    )
}

/// How the emails went, as one line with a dialog behind it: who got what,
/// the reason for each failure and a resend button. Nothing when nobody in
/// the group gave an email.
fn mail_strip(g: &Group, admin_path: &str) -> Markup {
    let people: Vec<(&str, &MailState)> = g
        .members
        .iter()
        .filter(|m| !m.email.is_empty())
        .map(|m| (m.name.as_str(), &m.mail))
        .chain((!g.host_email.is_empty()).then_some((g.host.as_str(), &g.host_mail)))
        .collect();
    if people.is_empty() {
        return html! {};
    }
    let count = |f: fn(&MailState) -> bool| people.iter().filter(|(_, s)| f(s)).count();
    let sent = count(|s| *s == MailState::Sent);
    let waiting = count(|s| matches!(s, MailState::Pending | MailState::Sending));
    let failed: Vec<(&str, &str)> = people
        .iter()
        .filter_map(|(n, s)| match s {
            MailState::Failed(why) => Some((*n, why.as_str())),
            _ => None,
        })
        .collect();
    let summary = html! {
        (sent) " sent"
        @if waiting > 0 { " · " (waiting) " sending" }
        @if !failed.is_empty() { " · " (failed.len()) " failed" }
    };
    html! {
        div.strip.bad[!failed.is_empty()] #mail {
            @if failed.is_empty() { (icon(CHECK)) } @else { (icon(WARN)) }
            p { strong { "Emails" } " " (summary) }
            button.btn.small type="button" command="show-modal" commandfor="mail-dialog" { (icon(MAIL)) "Details" }
        }
        dialog #mail-dialog aria-labelledby="mail-title" {
            h2 #mail-title { "Emails" }
            p.muted { (summary) }
            @if !failed.is_empty() {
                div.problem role="alert" {
                    (icon(WARN))
                    span {
                        strong { "Some emails didn’t go out" }
                        @for (name, why) in &failed {
                            span.mailerr { (name) ": " (why) }
                        }
                    }
                }
                form method="post" action=(format!("{admin_path}/resend")) {
                    button.btn type="submit" { (icon(UNDO)) "Resend failed emails" }
                }
            }
            ul.rows {
                @for (name, state) in &people {
                    li {
                        span.name title=(name) { (name) }
                        @match state {
                            MailState::Sent => span.pill.yes { "Sent" },
                            MailState::Failed(_) => span.pill.bad { "Failed" },
                            _ => span.pill { "Sending" },
                        }
                    }
                }
            }
            form method="dialog" { button.btn.small type="submit" { (icon(CROSS)) "Close" } }
        }
    }
}

/// One address may ask for its links this often, and one peer this often.
const LOCKOUT_EMAIL_SECS: u64 = 10 * 60;
const LOCKOUT_PEER_SECS: u64 = 10;

/// Whether `key` may ask now; if so, remembers that it did.
fn lockout_allowed(app: &App, key: String, gap: u64, at: u64) -> bool {
    let mut seen = app.lockout.lock().unwrap_or_else(|e| e.into_inner());
    if seen.len() > 10_000 {
        seen.retain(|_, last| at.saturating_sub(*last) < LOCKOUT_EMAIL_SECS);
    }
    match seen.get(&key) {
        Some(last) if at.saturating_sub(*last) < gap => false,
        _ => {
            seen.insert(key, at);
            true
        }
    }
}

fn locked_out(app: &App, error: Option<&str>, typed: &str) -> Res {
    page(locked_out_page(app.mail.is_some(), error, typed, false))
}

/// "Looking for a swap?": email whatever links belong to an address. The answer is
/// the same whether or not the address is in any group, so this can't be
/// used to find out who is.
fn locked_out_post(app: &App, peer: Option<IpAddr>, form: &Form) -> Res {
    if app.mail.is_none() {
        return locked_out(app, None, "");
    }
    let typed = form.get("email");
    let Some(email) = mail::clean_address(&typed) else {
        return locked_out(app, Some("That email address doesn’t look right."), &typed);
    };
    let at = now();
    if let Some(ip) = peer
        && !lockout_allowed(app, format!("peer {ip}"), LOCKOUT_PEER_SECS, at)
    {
        return locked_out(app, Some("Too many tries. Wait a few seconds."), &typed);
    }
    // A repeat for the same address within the gap sends nothing, quietly.
    if lockout_allowed(app, email.to_ascii_lowercase(), LOCKOUT_EMAIL_SECS, at) {
        let letter = {
            let st = lock(app);
            links_letter(&st.data, &email, app.public_url.as_deref())
        };
        if let Some(letter) = letter {
            app.outbox
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(letter);
            app.mail_wake.notify_one();
        }
    }
    page(locked_out_page(true, None, &email, true))
}

/// The email listing every link that belongs to `email`, by group, newest
/// first; `None` when the address isn't in any group.
fn links_letter(data: &Data, email: &str, public_url: Option<&str>) -> Option<mail::Letter> {
    let is = |s: &str| !s.is_empty() && s.eq_ignore_ascii_case(email);
    let mut groups: Vec<&Group> = data.groups.iter().rev().collect();
    groups.sort_by_key(|g| std::cmp::Reverse(g.created));
    let mut name = None;
    let mut blocks = Vec::new();
    for g in groups {
        let base = public_url.unwrap_or(&g.base);
        let mut lines = Vec::new();
        let mut me = g.members.iter().filter(|m| is(&m.email));
        let own = me.next();
        let hosting = is(&g.host_email) || own.is_some_and(|m| m.host);
        if !hosting && own.is_none() {
            continue;
        }
        if is(&g.host_email) {
            name.get_or_insert(g.host.as_str());
        }
        if let Some(m) = own {
            name.get_or_insert(m.name.as_str());
        }
        if hosting {
            lines.push(format!(
                "You host this one. Your admin link (keep it private):\n{base}/g/{}",
                g.admin
            ));
            if g.drawn.is_none() {
                lines.push(format!(
                    "Invite link, to send to the group:\n{base}/j/{}",
                    g.invite
                ));
            }
        }
        if let Some(m) = own {
            let what = if m.host {
                "Your own page, for your wishes and your gift tag"
            } else {
                "Your personal link, for your wishes and your gift tag"
            };
            lines.push(format!("{what}:\n{base}/p/{}", m.token));
        }
        let state = if g.drawn.is_some() {
            "names are drawn"
        } else {
            "names not drawn yet"
        };
        blocks.push(format!("{} ({state})\n\n{}", g.name, lines.join("\n\n")));
    }
    let name = name?;
    Some(mail::Letter {
        to_name: name.to_string(),
        to: email.to_string(),
        subject: "Your Kringle links".into(),
        text: format!(
            "Hi {},\n\nSomeone asked for the links to your gift swaps. Here they are, group by group. If that wasn’t you, ignore this email: nothing has changed.\n\n{}\n\nKeep these private: each link is the key to its page.\n\nKringle",
            first_word(name),
            blocks.join("\n\n— — —\n\n")
        ),
    })
}

fn locked_out_page(mail_on: bool, error: Option<&str>, typed: &str, sent: bool) -> Markup {
    let (_, _, eg_email) = example();
    layout(
        "Looking for a swap? · Kringle",
        html! { a.badge.red href="/" { (icon(GIFT_ICON)) "Start a swap" } },
        html! {
            section.panel.stack {
                h1 { "Looking for a swap?" }
                @if !mail_on {
                    p { "This Kringle can’t send email, so it can’t resend links. Ask whoever runs it, or ask your host for yours." }
                } @else if sent {
                    p.ok { (icon(CHECK)) span { "If " strong { (typed) } " is in a gift swap here, its links are on the way. Check your spam folder too." } }
                    p.muted.small { "You can ask again in a few minutes." }
                } @else {
                    p { "Enter the email you gave Kringle. We’ll send every link tied to it: admin links and invite links for groups you host, and your personal link for groups you joined." }
                    @if let Some(e) = error { p.error role="alert" { (e) } }
                    form.stack method="post" action="/locked-out" {
                        label.field {
                            span { "Your email" (star()) }
                            input type="email" name="email" maxlength="254" required placeholder=(eg_email) autocomplete="email" value=(typed);
                        }
                        button.btn.primary.big type="submit" { (icon(MAIL)) "Email me my links" }
                    }
                    p.muted.small { "Only works for groups where you gave an email." }
                }
                a href="/" { "Back to start" }
            }
        },
    )
}

fn deleted_page() -> Markup {
    layout(
        "Group deleted · Kringle",
        html! {},
        html! {
            section.panel {
                h1 { "Group deleted" }
                p { "The group, its wishes and its draw are gone, and its links no longer work." }
                a.btn href="/" { (icon(GIFT_ICON)) "Start a new gift swap" }
            }
        },
    )
}

fn join_page(
    g: &Group,
    returning: Option<&Member>,
    f: &JoinForm,
    error: Option<&str>,
    ask_email: bool,
) -> Markup {
    let n = g.members.len();
    let (eg_name, _, eg_email) = example();
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
                    a.btn.primary.big href=(format!("/p/{}", m.token)) { "Open my page" (icon(ARROW_RIGHT)) }
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
                        span { "Your name" (star()) }
                        input type="text" name="name" maxlength=(NAME_MAX) required placeholder=(eg_name) autocomplete="given-name" value=(f.name);
                        span.hint { "Use the name the group knows you by. The host uses it to set “keep apart” rules." }
                    }
                    @if ask_email {
                        label.field {
                            @if g.require_email {
                                span { "Your email" (star()) }
                            } @else {
                                span.row { "Your email" span.hint { "optional" } }
                            }
                            input type="email" name="email" maxlength="254" required[g.require_email] placeholder=(eg_email) autocomplete="email" value=(f.email);
                            span.hint { "We email you your personal link, and again when names are drawn. Only you get these emails." }
                        }
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
                button.btn type="submit" { (icon(CHECK)) "Save wishes" }
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
                            button.btn.primary type="button" data-copy=(my_url) { (icon(COPY)) span { "Copy" } }
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
                        a.btn href=(format!("/p/{}", m.token)) { (icon(BACK)) "Hide it again" }
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

macro_rules! glyph {
    ($path:literal) => {
        concat!(
            r##"<svg class="i" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">"##,
            $path,
            "</svg>"
        )
    };
}
const SHARE: &str = glyph!(r#"<circle cx="18" cy="5" r="3"/><circle cx="6" cy="12" r="3"/><circle cx="18" cy="19" r="3"/><path d="M8.6 13.5l6.8 4M15.4 6.5l-6.8 4"/>"#);
const USERS: &str = glyph!(r#"<circle cx="9" cy="8" r="3.5"/><path d="M2.5 20c0-3.6 2.9-6 6.5-6s6.5 2.4 6.5 6"/><path d="M16 4.6a3.5 3.5 0 0 1 0 6.8M18.5 14.4c2 .7 3.5 2.5 3.5 5.6"/>"#);
const MAIL: &str = glyph!(r#"<rect x="3" y="5" width="18" height="14" rx="2"/><path d="M3 7l9 6 9-6"/>"#);
const TRASH: &str = glyph!(r#"<path d="M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3"/>"#);
const COPY: &str = glyph!(r#"<rect x="9" y="9" width="11" height="11" rx="2"/><path d="M5 15V6a2 2 0 0 1 2-2h9"/>"#);
const SHUFFLE: &str = glyph!(r#"<path d="M3 7h4l10 10h4M3 17h4l3-3M14 10l3-3h4"/><path d="M19 5l2 2-2 2M19 15l2 2-2 2"/>"#);
const PLUS: &str = glyph!(r#"<path d="M12 5v14M5 12h14"/>"#);
const SEARCH: &str = glyph!(r#"<circle cx="11" cy="11" r="7"/><path d="M20 20l-4-4"/>"#);
const UNDO: &str = glyph!(r#"<path d="M3 12a9 9 0 1 0 3-6.7L3 8"/><path d="M3 3v5h5"/>"#);
const BACK: &str = glyph!(r#"<path d="M19 12H5M11 6l-6 6 6 6"/>"#);
const ARROW_RIGHT: &str = glyph!(r#"<path d="M5 12h14M13 6l6 6-6 6"/>"#);
const QR: &str = glyph!(r#"<rect x="3" y="3" width="7" height="7"/><rect x="14" y="3" width="7" height="7"/><rect x="3" y="14" width="7" height="7"/><path d="M14 14h3v3M21 14v.01M14 21h.01M17 21h4v-4"/>"#);
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
// End-to-end tests over the real router
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> (Shared, PathBuf) {
        test_app_with(None)
    }

    fn test_app_with(mail: Option<mail::Transport>) -> (Shared, PathBuf) {
        let path = std::env::temp_dir().join(format!("kringle-test-{}.json", token()));
        let app = Arc::new(App {
            store: Mutex::new(Store::load(path.clone()).unwrap()),
            public_url: Some("http://kringle.test".into()),
            keep_secs: 86_400,
            limiter: Mutex::new(HashMap::new()),
            mail,
            mail_wake: tokio::sync::Notify::new(),
            outbox: Mutex::new(Vec::new()),
            lockout: Mutex::new(HashMap::new()),
        });
        (app, path)
    }

    /// A response, flattened for easy assertions.
    struct Reply {
        status: StatusCode,
        location: Option<String>,
        set_cookie: Option<String>,
        content_type: String,
        encoding: Option<String>,
        body: Bytes,
    }

    impl Reply {
        /// Where a redirect goes, without the #fragment (not part of a request).
        fn to(&self) -> String {
            let loc = self.location.as_deref().expect("a redirect");
            loc.split('#').next().unwrap().to_string()
        }
        fn has(&self, text: &str) -> bool {
            String::from_utf8_lossy(&self.body).contains(text)
        }
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.body).into_owned()
        }
    }

    /// Send a request through the same routing the server uses.
    async fn req(app: &Shared, method: &str, uri: &str, form: &str, cookie: Option<&str>) -> Reply {
        req_from(app, None, method, uri, form, cookie).await
    }

    async fn req_from(
        app: &Shared,
        peer: Option<IpAddr>,
        method: &str,
        uri: &str,
        form: &str,
        cookie: Option<&str>,
    ) -> Reply {
        let mut b = Request::builder().method(method).uri(uri);
        if method == "POST" {
            b = b.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        }
        if let Some(c) = cookie {
            b = b.header(header::COOKIE, c);
        }
        let res = handle(
            app,
            peer,
            b.body(Full::new(Bytes::from(form.to_string()))).unwrap(),
        )
        .await;
        let header = |k| {
            res.headers()
                .get(k)
                .map(|v: &HeaderValue| v.to_str().unwrap().to_string())
        };
        Reply {
            status: res.status(),
            location: header(header::LOCATION),
            set_cookie: header(header::SET_COOKIE),
            content_type: header(header::CONTENT_TYPE).unwrap_or_default(),
            encoding: header(header::CONTENT_ENCODING),
            body: res.into_body().collect().await.unwrap().to_bytes(),
        }
    }

    async fn get(r: &Shared, uri: &str) -> Reply {
        req(r, "GET", uri, "", None).await
    }

    async fn post(r: &Shared, uri: &str, form: &str) -> Reply {
        req(r, "POST", uri, form, None).await
    }

    fn between<'a>(s: &'a str, start: &str, end: &str) -> &'a str {
        let i = s.find(start).unwrap() + start.len();
        &s[i..i + s[i..].find(end).unwrap()]
    }

    /// Start a group and return (admin path, invite path).
    async fn start(r: &Shared, form: &str) -> (String, String) {
        let admin = post(r, "/groups", form).await.to();
        let page = get(r, &admin).await.text();
        let invite = format!("/j/{}", between(&page, "http://kringle.test/j/", "\""));
        (admin, invite)
    }

    async fn join(r: &Shared, invite: &str, form: &str) -> String {
        let res = post(r, invite, form).await;
        assert_eq!(
            res.status,
            StatusCode::SEE_OTHER,
            "join {form}: {}",
            res.text()
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
                && !res.has("/draw")
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
            res.has("aria-label=\"can’t give to\"") && res.has("Draw settings · 2 rules")
        );
        post(&r, &format!("{admin}/rules/1/delete"), "").await;
        assert!(!get(&r, &admin).await.has("aria-label=\"can’t give to\""));

        // Remove a person: their rules go too; the host can't be removed.
        post(&r, &format!("{admin}/people/3/delete"), "").await;
        post(&r, &format!("{admin}/people/1/delete"), "").await;
        let res = get(&r, &admin).await;
        assert!(!res.has("title=\"Sam\"") && res.has("Jamie (you)"));
        assert!(
            !res.has("can’t give to each other") && res.has("Draw settings (optional)")
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
            let text = res.text();
            let full = between(
                &text,
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
    async fn text_assets_are_served_gzipped_and_match_their_sources() {
        use std::io::Read as _;
        let (r, path) = test_app();
        for (file, source) in [
            ("style.css", &include_bytes!("../static/style.css")[..]),
            ("app.js", &include_bytes!("../static/app.js")[..]),
            ("town.svg", &include_bytes!("../static/town.svg")[..]),
            ("clouds.svg", &include_bytes!("../static/clouds.svg")[..]),
        ] {
            let res = get(&r, &format!("/static/{file}")).await;
            assert_eq!(res.encoding.as_deref(), Some("gzip"), "{file}");
            let mut plain = Vec::new();
            flate2::read::GzDecoder::new(&res.body[..])
                .read_to_end(&mut plain)
                .unwrap();
            assert_eq!(plain, source, "{file}");
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

    #[tokio::test]
    async fn oversized_forms_are_refused() {
        let (r, path) = test_app();
        let big = format!("group={}&host=Rosa", "x".repeat(BODY_MAX));
        assert_eq!(
            post(&r, "/groups", &big).await.status,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert!(lock(&r).data.groups.is_empty(), "nothing was created");
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn stronger_rules_upgrade_instead_of_being_refused() {
        let (r, path) = test_app();
        let (admin, invite) = start(&r, "group=Flat&host=Jo").await;
        for name in ["Alex", "Sam", "Kim", "Lee"] {
            join(&r, &invite, &format!("name={name}")).await;
        }
        let rules = format!("{admin}/rules");
        let rule_of = |r: &Shared, a: u32, b: u32| {
            let st = lock(r);
            let g = &st.data.groups[0];
            g.rules
                .iter()
                .find(|x| (x.giver, x.receiver) == (a, b) || (x.giver, x.receiver) == (b, a))
                .map(|x| (x.giver, x.receiver, x.both))
        };
        // Members: Alex 1, Sam 2, Kim 3, Lee 4.
        // One way Sam -> Alex, then Alex <-> Sam: upgraded, not refused.
        post(&r, &rules, "giver=2&receiver=1").await;
        assert_eq!(
            post(&r, &rules, "giver=1&receiver=2&both=1").await.to(),
            admin
        );
        assert_eq!(rule_of(&r, 1, 2), Some((2, 1, true)));
        let a = lock(&r).data.groups[0].allowed();
        assert!(!a[0][1] && !a[1][0], "Alex and Sam can't draw each other");
        // One way Kim -> Lee, then Lee -> Kim one way: that's both ways too.
        post(&r, &rules, "giver=3&receiver=4").await;
        assert_eq!(post(&r, &rules, "giver=4&receiver=3").await.to(), admin);
        assert_eq!(rule_of(&r, 3, 4), Some((3, 4, true)));
        // Anything already covered is still "already exists".
        assert!(
            post(&r, &rules, "giver=1&receiver=2")
                .await
                .to()
                .ends_with("?err=dup")
        );
        assert!(
            post(&r, &rules, "giver=4&receiver=3&both=1")
                .await
                .to()
                .ends_with("?err=dup")
        );
        assert_eq!(lock(&r).data.groups[0].rules.len(), 2);
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn starting_groups_is_rate_limited_per_address() {
        let (r, path) = test_app();
        let a: IpAddr = "192.0.2.1".parse().unwrap();
        let b: IpAddr = "192.0.2.2".parse().unwrap();
        for _ in 0..CREATE_BURST {
            let res = req_from(&r, Some(a), "POST", "/groups", "group=G&host=H", None).await;
            assert_eq!(res.status, StatusCode::SEE_OTHER);
        }
        let res = req_from(&r, Some(a), "POST", "/groups", "group=G&host=H", None).await;
        assert_eq!(res.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(res.has("Try again in a little while"));
        let res = req_from(&r, Some(b), "POST", "/groups", "group=G&host=H", None).await;
        assert_eq!(
            res.status,
            StatusCode::SEE_OTHER,
            "other addresses aren't affected"
        );
        // The allowance comes back over time.
        assert!(r.may_create(Some(a), now() + CREATE_EVERY_SECS));
        assert!(!r.may_create(Some(a), now() + CREATE_EVERY_SECS));
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn locked_out_emails_links_by_group_without_revealing_who_is_in() {
        let (r, path) = test_app_with(Some(mail::Transport::Log(Default::default())));
        let (a1, i1) = start(
            &r,
            "group=Office&host=Jo&plays=1&require_email=1&email=jo%40example.com",
        )
        .await;
        let lu = join(&r, &i1, "name=Lu&email=lu%40example.com").await;
        let (a2, i2) = start(
            &r,
            "group=Flat&host=Jo&require_email=1&email=JO%40example.com",
        )
        .await;
        deliver(&r).await;
        outbox(&r);

        assert!(get(&r, "/locked-out").await.has("Email me my links"));
        assert!(get(&r, "/").await.has("href=\"/locked-out\""));
        let bad = post(&r, "/locked-out", "email=nope").await;
        assert!(bad.has("doesn’t look right"));

        // Same answer for a stranger, and nothing is sent to them.
        let nobody = post(&r, "/locked-out", "email=who%40example.com").await;
        assert!(nobody.has("on the way"));
        deliver(&r).await;
        assert!(outbox(&r).is_empty());

        let res = post(&r, "/locked-out", "email=jo%40EXAMPLE.com").await;
        assert!(res.has("on the way"));
        deliver(&r).await;
        let sent = outbox(&r);
        assert_eq!(sent.len(), 1);
        let t = &sent[0].text;
        assert_eq!(sent[0].to, "jo@EXAMPLE.com");
        let (office, flat) = (t.find("Office").unwrap(), t.find("Flat").unwrap());
        assert!(flat < office, "newest group first");
        for link in [&a1, &i1, &a2, &i2] {
            assert!(t.contains(&format!("http://kringle.test{link}\n")), "{link}");
        }
        assert!(!t.contains(&lu), "other people's links stay private");

        // Asking again straight away sends nothing more.
        post(&r, "/locked-out", "email=jo%40example.com").await;
        deliver(&r).await;
        assert!(outbox(&r).is_empty());

        // A guest gets only their own link.
        post(&r, "/locked-out", "email=lu%40example.com").await;
        deliver(&r).await;
        let sent = outbox(&r);
        assert_eq!(sent.len(), 1);
        assert!(sent[0].text.contains(&format!("http://kringle.test{lu}\n")));
        assert!(!sent[0].text.contains(&a1) && !sent[0].text.contains(&i1));
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn locked_out_page_without_email_says_so() {
        let (r, path) = test_app();
        assert!(get(&r, "/locked-out").await.has("can’t send email"));
        assert!(post(&r, "/locked-out", "email=a%40b.co").await.has("can’t send email"));
        assert!(!get(&r, "/").await.has("/locked-out"));
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn host_can_delete_a_group() {
        let (r, path) = test_app();
        let (admin, invite) = start(&r, "group=Flat&host=Jo&plays=1").await;
        let alex = join(&r, &invite, "name=Alex").await;
        assert!(get(&r, &admin).await.has("Delete this group"));
        assert_eq!(
            get(&r, &format!("{admin}/delete")).await.status,
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(
            post(&r, &format!("{admin}/delete"), "").await.to(),
            "/deleted"
        );
        assert!(get(&r, "/deleted").await.has("Group deleted"));
        for link in [&admin, &invite, &alex] {
            assert_eq!(get(&r, link).await.status, StatusCode::NOT_FOUND, "{link}");
        }
        assert!(Store::load(path.clone()).unwrap().data.groups.is_empty());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn old_and_never_joined_groups_are_pruned() {
        let path = std::env::temp_dir().join(format!("kringle-test-{}.json", token()));
        let mut st = Store::load(path).unwrap();
        let group = |created: u64, guest: bool| Group {
            name: "G".into(),
            host: "H".into(),
            budget: String::new(),
            admin: token(),
            invite: token(),
            created,
            drawn: None,
            next_id: 3,
            members: [(1, true), (2, false)]
                .into_iter()
                .filter(|&(_, host)| host || guest)
                .map(|(id, host)| Member {
                    id,
                    name: format!("M{id}"),
                    wishes: String::new(),
                    token: token(),
                    host,
                    opened: false,
                    gives_to: None,
                    ..Default::default()
                })
                .collect(),
            rules: Vec::new(),
            ..Default::default()
        };
        let day = 86_400;
        let t = 1_000 * day;
        st.data.groups = vec![
            group(t - day, false),      // new and empty: kept
            group(t - 8 * day, false),  // a week old, nobody joined: removed
            group(t - 8 * day, true),   // a week old with a guest: kept
            group(t - 121 * day, true), // past keep_days: removed
        ];
        assert!(st.prune(t, 120 * day));
        let ages: Vec<u64> = st
            .data
            .groups
            .iter()
            .map(|g| (t - g.created) / day)
            .collect();
        assert_eq!(ages, [1, 8]);
        assert!(!st.prune(t, 120 * day), "nothing more to remove");
    }

    #[tokio::test]
    async fn data_file_is_private_and_failed_unwrap_is_rolled_back() {
        let (r, path) = test_app();
        let (admin, invite) = start(&r, "group=Flat&host=Jo").await;
        let people: Vec<String> = {
            let mut v = Vec::new();
            for n in ["Alex", "Sam", "Kim"] {
                v.push(join(&r, &invite, &format!("name={n}")).await);
            }
            v
        };
        post(&r, &format!("{admin}/draw"), "").await;
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "kringle.json is readable only by Kringle");

        // Make saving fail: point the store at a directory.
        let dir = std::env::temp_dir().join(format!("kringle-dir-{}", token()));
        fs::create_dir(&dir).unwrap();
        lock(&r).path = dir.clone();
        let res = post(&r, &format!("{}/open", people[0]), "").await;
        assert_eq!(res.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            !lock(&r).data.groups[0].members[0].opened,
            "unwrap rolled back"
        );
        assert!(get(&r, &admin).await.has("0 of 3 have opened"));
        let _ = fs::remove_dir_all(dir);
        let _ = fs::remove_file(path);
    }

    /// A request body that never arrives.
    struct Stalled;

    impl hyper::body::Body for Stalled {
        type Data = Bytes;
        type Error = Infallible;
        fn poll_frame(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, Infallible>>> {
            std::task::Poll::Pending
        }
    }

    #[tokio::test(start_paused = true)]
    async fn slow_form_bodies_time_out() {
        let (r, path) = test_app();
        let req = Request::builder()
            .method("POST")
            .uri("/groups")
            .body(Stalled)
            .unwrap();
        let res = handle(&r, None, req).await;
        assert_eq!(res.status(), StatusCode::REQUEST_TIMEOUT);
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

    // -----------------------------------------------------------------------
    // Email
    // -----------------------------------------------------------------------

    fn log_mail() -> Option<mail::Transport> {
        Some(mail::Transport::Log(Default::default()))
    }

    /// Everything the log transport has "sent" so far, and forget it.
    fn outbox(r: &Shared) -> Vec<mail::Letter> {
        match &r.mail {
            Some(mail::Transport::Log(o)) => std::mem::take(&mut *o.lock().unwrap()),
            _ => panic!("not a log transport"),
        }
    }

    fn enc(s: &str) -> String {
        form_urlencoded::byte_serialize(s.as_bytes()).collect()
    }

    #[tokio::test]
    async fn required_emails_from_create_to_draw() {
        let (r, path) = test_app_with(log_mail());

        // The form offers emails, with "Require emails" on by default.
        let res = get(&r, "/").await;
        assert!(res.has("Your email") && res.has(r#"name="require_email" value="1" checked"#));

        // Required: the host must give a valid one.
        let res = post(
            &r,
            "/groups",
            "group=Office&host=Rosa+Diaz&plays=1&require_email=1",
        )
        .await;
        assert!(res.has("Add your email."), "{}", res.text());
        let res = post(
            &r,
            "/groups",
            "group=Office&host=Rosa&plays=1&require_email=1&email=rosa%40nowhere",
        )
        .await;
        assert!(res.has("doesn’t look right"));

        let (admin, invite) = start(
            &r,
            &format!(
                "group=Office&host=Rosa+Diaz&plays=1&require_email=1&email={}&budget=25",
                enc(" rosa@example.com ")
            ),
        )
        .await;
        deliver(&r).await;
        let sent = outbox(&r);
        assert_eq!(sent.len(), 1, "only the host so far");
        let host_mail = &sent[0];
        assert_eq!(
            (host_mail.to.as_str(), host_mail.to_name.as_str()),
            ("rosa@example.com", "Rosa Diaz")
        );
        assert_eq!(host_mail.subject, "Your gift swap Office is ready");
        assert!(host_mail.text.starts_with("Hi Rosa,"));
        assert!(
            host_mail
                .text
                .contains(&format!("http://kringle.test{admin}")),
            "admin link"
        );
        assert!(
            host_mail
                .text
                .contains(&format!("http://kringle.test{invite}")),
            "invite link"
        );
        assert!(
            host_mail.text.contains("http://kringle.test/p/"),
            "her own page: she plays"
        );

        // Guests must give one too; the form says so.
        let page = get(&r, &invite).await;
        assert!(page.has(r#"name="email""#) && page.has("required"));
        let res = post(&r, &invite, "name=Sam").await;
        assert!(res.has("Add your email."));
        let res = post(&r, &invite, "name=Sam&email=sam").await;
        assert!(
            res.has("doesn’t look right") && res.has(r#"value="Sam""#),
            "keeps what they typed"
        );

        let sam = join(&r, &invite, "name=Sam&email=sam%40example.org").await;
        let kim = join(&r, &invite, "name=Kim&email=kim%40tuta.com").await;
        deliver(&r).await;
        let sent = outbox(&r);
        // Each guest gets their own link; the host gets nothing when people join.
        assert_eq!(
            sent.iter().map(|l| l.to.as_str()).collect::<Vec<_>>(),
            ["sam@example.org", "kim@tuta.com"]
        );
        assert_eq!(sent[0].subject, "You’re in Office");
        assert!(sent[0].text.contains(&format!("http://kringle.test{sam}")));
        assert!(sent[1].text.contains(&format!("http://kringle.test{kim}")));
        assert!(
            !sent[0].text.contains(&admin),
            "guests never see the admin link"
        );

        // The host sees how emails went, never the addresses.
        let page = get(&r, &admin).await;
        assert!(page.has("3 sent"), "{}", page.text());
        assert!(!page.has("sam@example.org") && !page.has("Resend"));

        // Drawing names emails everyone a link to unwrap their tag.
        post(&r, &format!("{admin}/draw"), "").await;
        deliver(&r).await;
        let sent = outbox(&r);
        assert_eq!(sent.len(), 3);
        for l in &sent {
            assert_eq!(l.subject, "Names are drawn for Office");
            assert!(l.text.contains("Unwrap your gift tag") && l.text.contains("$25"));
        }
        let to_sam = sent.iter().find(|l| l.to == "sam@example.org").unwrap();
        assert!(to_sam.text.contains(&format!("http://kringle.test{sam}")));
        // The link works and shows the tag.
        assert!(get(&r, &sam).await.has("Unwrap"));

        // Nothing is left over, and the status survives a restart.
        deliver(&r).await;
        assert!(outbox(&r).is_empty());
        let saved = Store::load(path.clone()).unwrap();
        assert!(
            saved.data.groups[0]
                .members
                .iter()
                .all(|m| m.mail == MailState::Sent)
        );
        assert!(saved.data.groups[0].require_email);
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn optional_emails_and_a_host_who_does_not_play() {
        let (r, path) = test_app_with(log_mail());
        // "Use emails" unticked: the host's email box is hidden, so one sent
        // anyway is ignored; guests may give one.
        let (_, invite) = start(&r, "group=Club&host=Ann&email=ann%40example.com").await;
        let page = get(&r, &invite).await;
        assert!(
            page.has("optional")
                && !page.has(r#"type="email" name="email" maxlength="254" required"#)
        );
        join(&r, &invite, "name=Bo").await;
        join(&r, &invite, "name=Cy&email=cy%40example.com").await;
        deliver(&r).await;
        let sent = outbox(&r);
        assert_eq!(
            sent.iter().map(|l| l.to.as_str()).collect::<Vec<_>>(),
            ["cy@example.com"],
            "no email for the host"
        );

        // A host who isn't playing still gets the admin link, and no email
        // at the draw (they have no tag).
        let (admin, invite) = start(
            &r,
            "group=Team&host=Ann&require_email=1&email=ann%40example.com",
        )
        .await;
        for n in ["Bo", "Cy", "Di"] {
            join(&r, &invite, &format!("name={n}&email={n}%40example.com")).await;
        }
        deliver(&r).await;
        let sent = outbox(&r);
        assert_eq!(sent[0].to, "ann@example.com");
        assert!(
            sent[0].text.contains(&admin) && !sent[0].text.contains("/p/"),
            "admin link, no own page"
        );
        post(&r, &format!("{admin}/draw"), "").await;
        deliver(&r).await;
        let sent = outbox(&r);
        assert_eq!(
            sent.iter().map(|l| l.to.as_str()).collect::<Vec<_>>(),
            ["Bo@example.com", "Cy@example.com", "Di@example.com"]
        );
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn without_email_settings_no_email_is_asked_for() {
        let (r, path) = test_app();
        assert!(!get(&r, "/").await.has("Your email"));
        // Even if a form sends it, nothing is required or kept.
        let (admin, invite) = start(&r, "group=G&host=H&plays=1&require_email=1").await;
        assert!(!get(&r, &invite).await.has(r#"name="email""#));
        join(&r, &invite, "name=A&email=a%40example.com").await;
        {
            let st = lock(&r);
            let g = &st.data.groups[0];
            assert!(!g.require_email && g.members.iter().all(|m| m.email.is_empty()));
        }
        assert!(!get(&r, &admin).await.has("Emails"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn groups_saved_before_emails_still_load() {
        let path = std::env::temp_dir().join(format!("kringle-test-{}.json", token()));
        fs::write(
            &path,
            r#"{"groups":[{"name":"Old","host":"H","budget":"","admin":"a","invite":"i","created":1,"drawn":null,"next_id":2,
                "members":[{"id":1,"name":"H","wishes":"","token":"t","host":true,"opened":false,"gives_to":null}],"rules":[]}]}"#,
        )
        .unwrap();
        let st = Store::load(path.clone()).unwrap();
        let g = &st.data.groups[0];
        assert!(!g.require_email, "old groups don't suddenly require emails");
        assert_eq!(g.members[0].mail, MailState::None);
        let _ = fs::remove_file(path);
    }

    /// A tiny SMTP server: accepts mail, except that while `reject` is set it
    /// refuses every recipient the way a real server refuses a bad mailbox.
    async fn fake_smtp(
        reject: Arc<std::sync::atomic::AtomicBool>,
    ) -> (u16, Arc<Mutex<Vec<String>>>) {
        use std::sync::atomic::Ordering;
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let got = Arc::new(Mutex::new(Vec::new()));
        let inbox = got.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (read, mut write) = stream.into_split();
                let mut lines = BufReader::new(read).lines();
                write.write_all(b"220 fake ESMTP\r\n").await.unwrap();
                let mut data: Option<String> = None;
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Some(body) = &mut data {
                        if line == "." {
                            inbox.lock().unwrap().push(data.take().unwrap());
                            write.write_all(b"250 queued\r\n").await.unwrap();
                        } else {
                            body.push_str(&line);
                            body.push('\n');
                        }
                        continue;
                    }
                    let verb = line.get(..4).unwrap_or("").to_ascii_uppercase();
                    let reply: &[u8] = match verb.as_str() {
                        "RCPT" if reject.load(Ordering::SeqCst) => {
                            b"550 5.1.1 No such mailbox here\r\n"
                        }
                        "DATA" => {
                            data = Some(String::new());
                            b"354 go ahead\r\n"
                        }
                        "QUIT" => {
                            let _ = write.write_all(b"221 bye\r\n").await;
                            break;
                        }
                        _ => b"250 ok\r\n",
                    };
                    write.write_all(reply).await.unwrap();
                }
            }
        });
        (port, got)
    }

    #[tokio::test]
    async fn a_failed_email_is_explained_and_can_be_resent_over_smtp() {
        let reject = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (port, inbox) = fake_smtp(reject.clone()).await;
        let vars = HashMap::from([
            ("KRINGLE_MAIL", "smtp".to_string()),
            ("KRINGLE_SMTP_HOST", "127.0.0.1".into()),
            ("KRINGLE_SMTP_PORT", port.to_string()),
            ("KRINGLE_SMTP_SECURITY", "none".into()),
            ("KRINGLE_MAIL_FROM", "kringle@example.com".into()),
        ]);
        let transport = mail::Transport::from_vars(|k| vars.get(k).cloned()).unwrap();
        let (r, path) = test_app_with(transport);

        let (admin, invite) = start(
            &r,
            "group=Team&host=Jo&plays=1&require_email=1&email=jo%40example.com",
        )
        .await;
        join(&r, &invite, "name=Lu&email=lu%40example.com").await;
        deliver(&r).await;

        // The host sees who missed out and why, and the log says the same.
        let page = get(&r, &admin).await;
        assert!(
            page.has("Some emails didn’t go out") && page.has("2 failed"),
            "{}",
            page.text()
        );
        assert!(
            page.has("Jo: SMTP") && page.has("No such mailbox here"),
            "{}",
            page.text()
        );
        assert!(inbox.lock().unwrap().is_empty());
        // LuCI sees only counts.
        let status = mail_status(&lock(&r).data);
        assert_eq!(
            status,
            r#"{"failed":2,"groups_with_failures":1,"sent":0,"waiting":0}"#
        );
        assert!(!status.contains("Jo") && !status.contains("mailbox"));

        // Fixed on the server's side: resending gets them out.
        reject.store(false, std::sync::atomic::Ordering::SeqCst);
        let res = post(&r, &format!("{admin}/resend"), "").await;
        assert_eq!(res.status, StatusCode::SEE_OTHER);
        deliver(&r).await;
        let page = get(&r, &admin).await;
        assert!(page.has("2 sent") && !page.has("failed"), "{}", page.text());
        assert_eq!(
            mail_status(&lock(&r).data),
            r#"{"failed":0,"groups_with_failures":0,"sent":2,"waiting":0}"#
        );
        let got = inbox.lock().unwrap().clone();
        assert_eq!(got.len(), 2);
        assert!(
            got[0].contains("Subject: Your gift swap Team is ready"),
            "{}",
            got[0]
        );
        assert!(
            got[0].contains("To: Jo <jo@example.com>")
                && got[0].contains("From: Kringle <kringle@example.com>")
        );
        assert!(got[1].contains("To: Lu <lu@example.com>"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn email_settings_are_checked_at_startup() {
        let check = |pairs: &[(&str, &str)]| {
            let vars: HashMap<String, String> = pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            mail::Transport::from_vars(|k| vars.get(k).cloned()).map(|t| t.map(|t| t.describe()))
        };
        assert_eq!(check(&[]), Ok(None), "off unless asked for");
        assert_eq!(check(&[("KRINGLE_MAIL", "off")]), Ok(None));
        assert_eq!(
            check(&[("KRINGLE_MAIL", "smtp")]),
            Err("KRINGLE_SMTP_HOST is not set".into())
        );
        assert!(
            check(&[
                ("KRINGLE_MAIL", "smtp"),
                ("KRINGLE_SMTP_HOST", "h"),
                ("KRINGLE_SMTP_SECURITY", "ssl")
            ])
            .unwrap_err()
            .contains("tls, starttls or none")
        );
        assert!(
            check(&[
                ("KRINGLE_MAIL", "smtp"),
                ("KRINGLE_SMTP_HOST", "h"),
                ("KRINGLE_MAIL_FROM", "nope")
            ])
            .unwrap_err()
            .contains("KRINGLE_MAIL_FROM")
        );
        assert_eq!(
            check(&[
                ("KRINGLE_MAIL", "smtp"),
                ("KRINGLE_SMTP_HOST", "smtp.example.com"),
                ("KRINGLE_SMTP_USER", "me@example.com")
            ]),
            Err("KRINGLE_SMTP_PASSWORD is not set".into()),
        );
        assert_eq!(
            check(&[
                ("KRINGLE_MAIL", "smtp"),
                ("KRINGLE_SMTP_HOST", "smtp.example.com"),
                ("KRINGLE_SMTP_USER", "me@example.com"),
                ("KRINGLE_SMTP_PASSWORD", "pw"),
            ]),
            Ok(Some(
                "SMTP via smtp.example.com:465 (tls) as kringle@hollingworth.xyz".into()
            )),
            "implicit TLS on 465 by default, sending as kringle@hollingworth.xyz"
        );
        assert_eq!(
            check(&[
                ("KRINGLE_MAIL", "smtp"),
                ("KRINGLE_SMTP_HOST", "h"),
                ("KRINGLE_SMTP_SECURITY", "starttls"),
                ("KRINGLE_MAIL_FROM", "a@b.c")
            ]),
            Ok(Some("SMTP via h:587 (starttls) as a@b.c".into())),
        );
        assert!(
            check(&[
                ("KRINGLE_MAIL", "smtp"),
                ("KRINGLE_SMTP_HOST", "h"),
                ("KRINGLE_SMTP_PORT", "x"),
                ("KRINGLE_MAIL_FROM", "a@b.c")
            ])
            .unwrap_err()
            .contains("port number")
        );
        assert_eq!(
            check(&[
                ("KRINGLE_MAIL", "tuta"),
                ("KRINGLE_TUTA_USER", "k@tuta.com")
            ]),
            Err("KRINGLE_TUTA_PASSWORD is not set".into())
        );
        assert_eq!(
            check(&[
                ("KRINGLE_MAIL", "tuta"),
                ("KRINGLE_TUTA_USER", "k@tuta.com"),
                ("KRINGLE_TUTA_PASSWORD", "pw")
            ]),
            Ok(Some("Tuta as k@tuta.com, from kringle@hollingworth.xyz".into()))
        );
        assert!(
            check(&[("KRINGLE_MAIL", "gmail")])
                .unwrap_err()
                .contains("off, smtp, tuta or log")
        );
    }

    #[test]
    fn tuta_mail_bodies_are_escaped_html_with_links() {
        assert_eq!(
            mail::html("Hi <b>Al</b> & co,\nline two\n\nhttps://x.test/p/abc?q=1&r=2"),
            "<p>Hi &lt;b&gt;Al&lt;/b&gt; &amp; co,<br>line two</p><p><a href=\"https://x.test/p/abc?q=1&amp;r=2\">https://x.test/p/abc?q=1&amp;r=2</a></p>"
        );
    }

    #[test]
    fn email_addresses_are_checked_for_typos() {
        assert_eq!(
            mail::clean_address("  a.b+c@mail.example.org "),
            Some("a.b+c@mail.example.org".into())
        );
        for bad in [
            "",
            "a",
            "a@b",
            "@b.c",
            "a@.b",
            "a@b.",
            "a b@c.d",
            "a@b@c.d",
            "a@b.c\r\nBcc: x@y.z",
            "<a@b.c>",
        ] {
            assert_eq!(mail::clean_address(bad), None, "{bad:?}");
        }
    }

    #[tokio::test]
    async fn an_expired_tuta_session_signs_in_again_once() {
        use std::cell::Cell;
        // Sessions are numbered by sign-in; `refuse` lists the sessions the
        // fake server rejects as expired, `broken` makes every send fail
        // for some other reason.
        async fn run(
            slot: &mut Option<u32>,
            (sign_ins, sends): (&Cell<u32>, &Cell<u32>),
            refuse: &[u32],
            broken: bool,
        ) -> Result<(), String> {
            mail::with_session(
                slot,
                || async {
                    sign_ins.set(sign_ins.get() + 1);
                    Ok(sign_ins.get())
                },
                |session| async move {
                    sends.set(sends.get() + 1);
                    if refuse.contains(&session) {
                        Err((true, format!("session {session} expired")))
                    } else if broken {
                        Err((false, "mailbox full".to_string()))
                    } else {
                        Ok(())
                    }
                },
            )
            .await
        }
        let (sign_ins, sends) = (Cell::new(0), Cell::new(0));
        let counters = (&sign_ins, &sends);
        let mut slot = None;
        // First email signs in; the next reuses the session.
        assert_eq!(run(&mut slot, counters, &[], false).await, Ok(()));
        assert_eq!(run(&mut slot, counters, &[], false).await, Ok(()));
        assert_eq!((sign_ins.get(), sends.get(), slot), (1, 2, Some(1)));
        // Session 1 expires: sign in again and the email still goes.
        assert_eq!(run(&mut slot, counters, &[1], false).await, Ok(()));
        assert_eq!((sign_ins.get(), sends.get(), slot), (2, 4, Some(2)));
        // Refused again straight after signing in: give up, no loop.
        assert_eq!(
            run(&mut slot, counters, &[2, 3], false).await,
            Err("session 3 expired".into())
        );
        assert_eq!((sign_ins.get(), sends.get(), slot), (3, 6, None));
        // Any other failure isn't retried, but the session is dropped.
        assert_eq!(
            run(&mut slot, counters, &[], true).await,
            Err("mailbox full".into())
        );
        assert_eq!((sign_ins.get(), sends.get(), slot), (4, 7, None));
    }

    #[tokio::test]
    async fn checking_the_email_settings_signs_in_without_sending() {
        let smtp = |port: u16| {
            let vars = HashMap::from([
                ("KRINGLE_MAIL", "smtp".to_string()),
                ("KRINGLE_SMTP_HOST", "127.0.0.1".into()),
                ("KRINGLE_SMTP_PORT", port.to_string()),
                ("KRINGLE_SMTP_SECURITY", "none".into()),
                ("KRINGLE_MAIL_FROM", "kringle@example.com".into()),
            ]);
            mail::Transport::from_vars(|k| vars.get(k).cloned())
                .unwrap()
                .unwrap()
        };
        let (port, inbox) = fake_smtp(Arc::new(false.into())).await;
        let ok = smtp(port).check().await.unwrap();
        assert!(
            ok.starts_with("Connected and signed in: SMTP via 127.0.0.1"),
            "{ok}"
        );
        assert!(inbox.lock().unwrap().is_empty(), "nothing sent");

        // Nothing listening: the reason says so.
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let why = smtp(closed).check().await.unwrap_err();
        assert!(
            why.starts_with("SMTP: ") && why.to_lowercase().contains("refused"),
            "{why}"
        );

        let log = log_mail().unwrap();
        assert!(log.check().await.unwrap().contains("system log"));
    }
}
