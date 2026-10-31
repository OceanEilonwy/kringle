//! Sending email: over SMTP (lettre, rustls with the bundled Mozilla roots)
//! or from a Tuta account (Tuta's own Rust SDK, vendored in `vendor/`; Tuta
//! has no SMTP). Settings come from the environment, which the OpenWrt init
//! script fills from Services › Kringle in LuCI.
//!
//! Sending is deliberately simple: one plain-text mail per person, sent one at
//! a time by a single background task (see `deliver` in main.rs). A failure is
//! logged and shown to the host with its reason, and the host can resend.

use std::{future::Future, sync::Arc, time::Duration};

use lettre::{
    AsyncSmtpTransport, AsyncTransport as _, Message, Tokio1Executor,
    message::{Mailbox, header::ContentType},
    transport::smtp::authentication::Credentials,
};

/// One email to one person.
#[derive(Clone, Debug, PartialEq)]
pub struct Letter {
    pub to_name: String,
    pub to: String,
    pub subject: String,
    /// Plain text. Lines that are only a link become links in HTML mail.
    pub text: String,
}

pub enum Transport {
    Smtp {
        relay: AsyncSmtpTransport<Tokio1Executor>,
        from: Mailbox,
        /// For the startup log line.
        describe: String,
    },
    Tuta(Tuta),
    /// Writes each mail to the log instead of sending it, and keeps the last
    /// few in memory. For trying Kringle out, and for the tests.
    Log(std::sync::Mutex<Vec<Letter>>),
}

const ENV_HELP: &str = "\
Email (all optional; set KRINGLE_MAIL to turn it on):
  KRINGLE_MAIL            off (default), smtp, tuta, or log (print mails to the log)
  KRINGLE_MAIL_FROM_NAME  sender name                    (default Kringle)
  SMTP:
  KRINGLE_SMTP_HOST       server, e.g. smtp.example.com
  KRINGLE_SMTP_PORT       default 465 for tls, 587 for starttls, 25 for none
  KRINGLE_SMTP_SECURITY   tls (default), starttls, or none
  KRINGLE_SMTP_USER       login, if the server needs one
  KRINGLE_SMTP_PASSWORD
  KRINGLE_MAIL_FROM       sender address                 (default KRINGLE_SMTP_USER)
  Tuta (signs in to a Tuta account and sends from it):
  KRINGLE_TUTA_USER       the account's address, e.g. kringle@tuta.com
  KRINGLE_TUTA_PASSWORD   its password (accounts with two-factor sign-in can't be used)
";

pub fn usage() -> &'static str {
    ENV_HELP
}

impl Transport {
    /// Read the mail settings from the environment. `Ok(None)` when email is
    /// off; `Err` explains a setting that is missing or wrong.
    pub fn from_env() -> Result<Option<Transport>, String> {
        Self::from_vars(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
    }

    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Result<Option<Transport>, String> {
        let need = |k: &str| var(k).ok_or_else(|| format!("{k} is not set"));
        let from_name = var("KRINGLE_MAIL_FROM_NAME").unwrap_or_else(|| "Kringle".into());
        match var("KRINGLE_MAIL").as_deref().map(str::trim) {
            None | Some("off") => Ok(None),
            Some("log") => Ok(Some(Transport::Log(Default::default()))),
            Some("smtp") => {
                let host = need("KRINGLE_SMTP_HOST")?;
                let security = var("KRINGLE_SMTP_SECURITY").unwrap_or_else(|| "tls".into());
                type Smtp = AsyncSmtpTransport<Tokio1Executor>;
                let (builder, default_port) = match security.as_str() {
                    "tls" => (Smtp::relay(&host), 465),
                    "starttls" => (Smtp::starttls_relay(&host), 587),
                    "none" => (Ok(Smtp::builder_dangerous(&host)), 25),
                    other => {
                        return Err(format!(
                            "KRINGLE_SMTP_SECURITY must be tls, starttls or none, not {other:?}"
                        ));
                    }
                };
                let mut b = builder.map_err(|e| format!("SMTP server {host:?}: {e}"))?;
                let port = match var("KRINGLE_SMTP_PORT") {
                    Some(p) => p.trim().parse::<u16>().map_err(|_| {
                        format!("KRINGLE_SMTP_PORT must be a port number, not {p:?}")
                    })?,
                    None => default_port,
                };
                b = b.port(port).timeout(Some(Duration::from_secs(30)));
                let user = var("KRINGLE_SMTP_USER");
                if let Some(user) = &user {
                    let password = need("KRINGLE_SMTP_PASSWORD")?;
                    b = b.credentials(Credentials::new(user.clone(), password));
                }
                let address = var("KRINGLE_MAIL_FROM")
                    .or(user.clone())
                    .ok_or("KRINGLE_MAIL_FROM is not set (and there's no KRINGLE_SMTP_USER to use instead)")?;
                let from = Mailbox::new(
                    Some(from_name),
                    address
                        .trim()
                        .parse()
                        .map_err(|e| format!("KRINGLE_MAIL_FROM {address:?}: {e}"))?,
                );
                Ok(Some(Transport::Smtp {
                    relay: b.build(),
                    describe: format!("SMTP via {host}:{port} ({security}) as {}", from.email),
                    from,
                }))
            }
            Some("tuta") => Ok(Some(Transport::Tuta(Tuta {
                user: need("KRINGLE_TUTA_USER")?.trim().to_string(),
                password: need("KRINGLE_TUTA_PASSWORD")?,
                from_name,
                session: Default::default(),
            }))),
            Some(other) => Err(format!(
                "KRINGLE_MAIL must be off, smtp, tuta or log, not {other:?}"
            )),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Transport::Smtp { describe, .. } => describe.clone(),
            Transport::Tuta(t) => format!("Tuta as {}", t.user),
            Transport::Log(_) => "printed to the log, not sent (KRINGLE_MAIL=log)".into(),
        }
    }

    /// Sign in to the SMTP server or Tuta without sending anything, for the
    /// "Check sign-in" button in LuCI. `Ok` says what worked.
    pub async fn check(&self) -> Result<String, String> {
        match self {
            Transport::Smtp {
                relay, describe, ..
            } => match relay.test_connection().await {
                Ok(true) => Ok(format!("Connected and signed in: {describe}.")),
                Ok(false) => Err(format!("Couldn't connect: {describe}.")),
                Err(e) => Err(format!("SMTP: {}", smtp_reason(&e))),
            },
            Transport::Tuta(t) => t.check().await,
            Transport::Log(_) => Ok("Emails are written to the system log, not sent.".into()),
        }
    }

    /// Before stopping: sign out of Tuta.
    pub async fn close(&self) {
        if let Transport::Tuta(t) = self {
            t.close().await;
        }
    }

    /// Send one letter. The error is a short reason for the log and the host.
    pub async fn send(&self, letter: &Letter) -> Result<(), String> {
        match self {
            Transport::Smtp { relay, from, .. } => {
                let to = letter
                    .to
                    .parse()
                    .map_err(|_| "that email address isn't valid".to_string())?;
                let message = Message::builder()
                    .from(from.clone())
                    .to(Mailbox::new(Some(letter.to_name.clone()), to))
                    .subject(&letter.subject)
                    .header(ContentType::TEXT_PLAIN)
                    .body(letter.text.clone())
                    .map_err(|e| format!("couldn't build the email: {e}"))?;
                relay
                    .send(message)
                    .await
                    .map(|_| ())
                    .map_err(|e| format!("SMTP: {}", smtp_reason(&e)))
            }
            Transport::Tuta(t) => t.send(letter).await,
            Transport::Log(outbox) => {
                eprintln!(
                    "kringle: email to {} <{}>\nSubject: {}\n\n{}\n",
                    letter.to_name, letter.to, letter.subject, letter.text
                );
                let mut outbox = outbox.lock().unwrap_or_else(|e| e.into_inner());
                if outbox.len() >= 100 {
                    outbox.remove(0);
                }
                outbox.push(letter.clone());
                Ok(())
            }
        }
    }
}

/// lettre's error with its causes, on one line: "connection refused" is in
/// the source, not the top-level message.
fn smtp_reason(e: &lettre::transport::smtp::Error) -> String {
    let mut out = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        let s = s.to_string();
        if !out.contains(&s) {
            out = format!("{out}: {s}");
        }
        source = source.and_then(|s| s.source());
    }
    out
}

/// A Tuta account. Signs in on the first mail and keeps the session; any
/// failure drops it, so the next mail signs in afresh.
pub struct Tuta {
    user: String,
    password: String,
    from_name: String,
    session: tokio::sync::Mutex<Option<Arc<tutasdk::LoggedInSdk>>>,
}

const TUTA_API: &str = "https://app.tuta.com";

type Sdk = Arc<tutasdk::LoggedInSdk>;

impl Tuta {
    async fn sign_in(&self) -> Result<Sdk, String> {
        let rest = Arc::new(
            tutasdk::net::native_rest_client::NativeRestClient::try_new()
                .map_err(|e| format!("Tuta: {e}"))?,
        );
        let files = Arc::new(tutasdk::bindings::test_file_client::TestFileClient::default());
        tutasdk::Sdk::new(TUTA_API.into(), rest, files)
            .create_session(&self.user, &self.password)
            .await
            .map_err(|e| format!("Tuta sign-in as {} failed: {e}", self.user))
    }

    async fn send(&self, letter: &Letter) -> Result<(), String> {
        let mut session = self.session.lock().await;
        let html = html(&letter.text);
        with_session(
            &mut session,
            || self.sign_in(),
            |sdk: Sdk| {
                let html = html.clone();
                async move {
                    sdk.send_mail(
                        &self.user,
                        &self.from_name,
                        &letter.to_name,
                        &letter.to,
                        &letter.subject,
                        &html,
                    )
                    .await
                    .map_err(|e| (signed_out(&e), format!("Tuta: {e}")))
                }
            },
        )
        .await
    }

    /// Sign in afresh and sign straight out again.
    async fn check(&self) -> Result<String, String> {
        let sdk = self.sign_in().await?;
        let _ = sdk.delete_session().await;
        Ok(format!("Signed in to Tuta as {}.", self.user))
    }

    /// Sign the kept session out, so sessions don't pile up in the account.
    async fn close(&self) {
        if let Some(sdk) = self.session.lock().await.take()
            && let Err(e) = sdk.delete_session().await
        {
            eprintln!("kringle: signing out of Tuta failed: {e}");
        }
    }
}

/// Whether Tuta refused the session itself (expired or signed out
/// elsewhere), so signing in again is worth one more try.
fn signed_out(e: &tutasdk::SendMailError) -> bool {
    use tutasdk::rest_error::HttpError::{NotAuthenticatedError, SessionExpiredError};
    matches!(
        e,
        tutasdk::SendMailError::Api(tutasdk::ApiCallError::ServerResponseError {
            source: NotAuthenticatedError | SessionExpiredError
        })
    )
}

/// Run `send` with the kept session, signing in first if there is none. Any
/// failure drops the session. If the session itself was refused (`send`
/// returns `(true, _)`), sign in again and try once more.
pub async fn with_session<S, SignIn, Send>(
    slot: &mut Option<S>,
    sign_in: impl Fn() -> SignIn,
    send: impl Fn(S) -> Send,
) -> Result<(), String>
where
    S: Clone,
    SignIn: Future<Output = Result<S, String>>,
    Send: Future<Output = Result<(), (bool, String)>>,
{
    for attempt in 0..2 {
        let session = match slot.clone() {
            Some(s) => s,
            None => {
                let s = sign_in().await?;
                *slot = Some(s.clone());
                s
            }
        };
        match send(session).await {
            Ok(()) => return Ok(()),
            Err((refused, why)) => {
                *slot = None;
                if !(refused && attempt == 0) {
                    return Err(why);
                }
            }
        }
    }
    unreachable!("the second attempt always returns")
}

/// Plain text as simple HTML (Tuta mail bodies are HTML): paragraphs, line
/// breaks, and lines that are a link become links.
pub fn html(text: &str) -> String {
    let esc = |s: &str| maud::html! { (s) }.into_string();
    text.split("\n\n")
        .map(|para| {
            let lines: Vec<String> = para
                .lines()
                .map(|l| {
                    if l.starts_with("http://") || l.starts_with("https://") {
                        format!("<a href=\"{0}\">{0}</a>", esc(l))
                    } else {
                        esc(l)
                    }
                })
                .collect();
            format!("<p>{}</p>", lines.join("<br>"))
        })
        .collect()
}

/// A trimmed email address if it looks like one, for catching typos on the
/// form. The SMTP server or Tuta has the final say.
pub fn clean_address(s: &str) -> Option<String> {
    let s = s.trim();
    let (local, domain) = s.split_once('@')?;
    let ok = s.len() <= 254
        && !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains('@')
        && !s
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "<>,;:\"()[]\\".contains(c));
    ok.then(|| s.to_string())
}
