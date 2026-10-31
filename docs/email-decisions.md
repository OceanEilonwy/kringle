# Email: decisions

Made while building email support (October 2026), with the asker away. The
brief: hosts choose "Require emails" (on by default); the host and guests
give emails; everyone gets their link by email on joining or creating the
group (the host gets nothing when someone joins); everyone gets an unwrap link
when names are drawn; SMTP set by environment or LuCI; Tuta must work; keep
it simple, make failures easy to diagnose, and give the host a resend button.

## Tuta

- **Tuta has no SMTP**, so Kringle signs in to a Tuta account with Tuta's own
  Rust SDK and sends from it, as asked ("use the rust-sdk with sign on").
- **The SDK has no send function.** It signs in and reads mail only. Kringle
  adds `send_mail` inside the SDK, following the web client's `createDraft` +
  `sendDraft`: Tuta recipients get end-to-end encrypted mail (bucket key
  sealed with their public key, RSA or TutaCrypt); everyone else gets
  ordinary mail delivered by Tuta.
- **Vendored, not a git dependency.** The tutanota repository is about
  300 MB, and the parts `send_mail` needs are private to the SDK crate. So
  `vendor/tuta-sdk/` holds the three crates (2.7 MB) with a few small,
  listed changes (`vendor/tuta-sdk/README.md`), including how to update.
- **Sign-in uses the SDK's `create_session`** (password; no two-factor).
  Upstream marks it "not ready yet for production use". It's the only
  sign-in the SDK has. The session is kept and reused, and dropped after any
  failure so the next email signs in afresh.
- **Not tested against a real account.** Without a Tuta login to use, I
  could only check that Tuta's API answers the SDK (a made-up address gets
  "Bad request" on sign-in, same as a plain request for it). **Before relying
  on it:** set up a throwaway Tuta account, choose "From a Tuta account" in
  LuCI, and send to a Tuta address and a non-Tuta address. If something is
  wrong, the host's page and `logread -e kringle` show the error.
- **Tuta's servers eventually refuse old clients.** The vendored SDK reports
  Tuta's client version 365.261030.0. When Tuta stops accepting it, refresh
  the vendor copy (steps in its README).

- **Staying signed in.** (Added after asking.) If Tuta refuses the session
  (401 or 440: expired or signed out elsewhere), Kringle signs in again and
  retries that email once, so an expired session doesn't cost an email or
  need a resend. Other failures aren't retried. Kringle signs the session
  out when it stops (3-second limit), so restarts don't leave sessions
  behind. The SDK's entity delete is `todo!()`, so signing out is a direct
  `DELETE /rest/sys/session/…`, as the web client does.

## LuCI: status and sign-in check

- **No new network endpoint.** LuCI runs `/etc/init.d/kringle mail_status`
  and `mail_check` through rpcd (allowed in the ACL; the check needs write
  access). They run the binary with `--mail-status` (reads the data file)
  and `--check-mail` (signs in with the saved settings, then signs out).
- The page shows **counts only**: failed emails and how many groups they're
  in, on the General tab's status line when any failed, and on the Email
  tab. Names and reasons stay on each host's page.
- **Check sign-in** uses the saved settings, so changes need Save & Apply
  first (the button says so). It gives up after 20 seconds. For SMTP it
  connects and logs in; for Tuta it signs in and out again.

## SMTP

- **lettre** (0.11.23) over **rustls with ring and the bundled Mozilla
  roots** (webpki-roots), as asked. No CA bundle is needed on the router.
- Security `tls` (465, default), `starttls` (587) or `none` (25, for a relay
  on the LAN). Login is optional; the sender address defaults to the login.

## Settings

- **Environment variables** (`KRINGLE_MAIL`, `KRINGLE_SMTP_*`,
  `KRINGLE_TUTA_*`, `KRINGLE_MAIL_FROM*`), no new command-line flags, so
  passwords never show in `ps`. The init script turns the UCI options from
  LuCI into those variables, one `procd_append_param env` each (values with
  spaces survive).
- **Email is off unless `KRINGLE_MAIL` is set.** With it off, no form asks
  for an email.
- **A wrong or missing setting stops Kringle at start-up** with the reason in
  the log, rather than failing later on each email.
- `KRINGLE_MAIL=log` prints each email to the log instead of sending it, for
  trying Kringle out. The tests use it too.
- LuCI gets an **Email** tab; passwords are password fields. The package
  installs the settings file readable by root only (0600).

## Behaviour

- **Require emails is stored per group**, on by default for new groups.
  Groups saved before this change load with it off, so nobody already in a
  group is stuck.
- With Require emails off, the email box is shown as optional, and anyone who
  fills it in still gets emails.
- **A host who isn't in the draw** gives their email on the group (required
  when Require emails is on, as agreed), gets the admin-link email, and isn't
  emailed at the draw (they have no tag).
- The host's email carries the admin link and invite link (and their own page
  if they play). Guests' emails never include the admin link.
- **The host never sees addresses**, only counts (sent / sending / failed)
  and, for failures, the person's name and the reason.
- **No email on cancelling a draw.** Drawing again sends everyone a new
  unwrap email.
- People can't change their email after joining. If it's wrong, the host
  removes them and they join again (before the draw).

## How sending works

- Each person's latest email state lives in the JSON file next to them:
  `pending`, `sent` or `failed` with the reason. One background task sends
  whatever is pending, one at a time; a restart carries on from the file.
  No separate queue.
- Which email goes out is decided at send time: once names are drawn it's
  the unwrap email, before that the welcome. If names are drawn while a
  welcome is going out, the unwrap email still follows.
- **Resend failed emails** on the host's page puts every failed one back to
  pending. There's no automatic retry.
- Emails are plain text over SMTP. Tuta needs HTML, so the same text is sent
  escaped, with links made clickable.

## Build

- Tuta's SDK brings C code (ring, the Kyber post-quantum code, BLAKE3), so
  the router build now uses **cargo-zigbuild** (0.23.4) with **zig**
  (ziglang 0.17.0 from PyPI) as the C cross compiler. CI installs both.
  rustls's default aws-lc backend is switched off (it needs cmake).
- The router binary grows from about 0.6 MB to about **2.9 MB** (TLS plus
  Tuta's crypto). The Flint 2 has plenty of room.
- Dependencies are held to versions released at least two weeks ago:
  22 crates the SDK pulled in at fresher versions were pinned back.
- **Version 0.2.0** (luci-app-kringle 0.1.2). Merging to `main` publishes the
  release.
