# Tuta SDK (vendored)

Tuta's Rust SDK, from <https://github.com/tutao/tutanota> (`tuta-sdk/rust/`),
commit `1f1272c63e0d591156650ab99778ffe50aa82afe` (30 October 2026), client
version 365.261030.0. GPL-3.0, like Kringle; see `LICENSE.txt`.

Kringle uses it to sign in to a Tuta account and send mail from it, because
Tuta has no SMTP. Only `sdk/`, `crypto-primitives/` and `util/` are here, with
their tests left out.

## What's changed from upstream

Kept small so updating stays easy:

- `sdk/src/send_mail.rs` (new): `LoggedInSdk::send_mail`, one mail to one
  recipient. Upstream's SDK can sign in and read mail but can't send; this
  follows the web client's `MailFacade.createDraft` and `sendDraft`
  (`src/applications/common/api/worker/facades/lazy/MailFacade.ts`).
- `sdk/src/lib.rs`: `LoggedInSdk` keeps the key loader, asymmetric crypto
  facade and public key provider that `send_mail` needs, and declares the
  module.
- `sdk/src/crypto/asymmetric_crypto_facade.rs`: `PubEncSymKey` fields are
  `pub(crate)`.
- `sdk/src/net/native_rest_client.rs`: the bundled Mozilla roots
  (`with_webpki_roots`) instead of the system's, since OpenWrt routers often
  have no CA bundle.
- The `Cargo.toml` files: workspace settings written out, `[dev-dependencies]`
  removed, and rustls / hyper-rustls on ring only (no aws-lc, which needs
  cmake) with the `webpki-roots` feature.

The crate version stays Tuta's (365.261030.0): the SDK sends it to Tuta's
servers as its client version.

## Updating

Tuta's servers stop accepting old client versions after a while. If Tuta
sign-in starts failing for every account, update this copy:

1. Copy `tuta-sdk/rust/{sdk,crypto-primitives,util}` from a new tutanota
   commit over these folders (leave out `sdk/tests`, `sdk/test_data` and
   `crypto-primitives/tests`), keeping `sdk/src/send_mail.rs`.
2. Redo the small changes listed above, set `version` in the three
   `Cargo.toml` files to the new workspace version, and update the commit
   here.
3. `cargo test`, then send a real email from a test Tuta account to both a
   Tuta address and a non-Tuta address.

`rustfmt.toml` is Tuta's, so formatting this folder keeps upstream's style.
