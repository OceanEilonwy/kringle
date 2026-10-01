# Kringle

A tiny Kris Kringle / Secret Santa gift swap server, built to run on a home
router (GL.iNet Flint 2 running OpenWrt 25.12).

A host starts a group and shares one invite link. People add their name and
some gift hints. The host can add "keep apart" rules (partners, housemates,
last year's pair) and then draws names. Everyone opens their own gift tag on
their personal page.

- **No accounts.** There are three kinds of unguessable link: admin (host
  only), invite (shared with everyone) and personal (one per person).
- **Secret by design.** The host sees who has opened their tag, never who got
  who. The draw is blocked if the rules would give a pair away, and the host
  is warned if the group is so small they could work it out from their own tag.
- **Small and light.** It's a static binary of about 0.5 MB with no runtime
  dependencies. It's fully async, so an idle router stays idle. The templates, CSS, fonts and artwork are compiled in. All
  state lives in one JSON file, rewritten atomically after each change.
- **Works without JavaScript.** Every action is a plain form. A few lines of
  JS add copy buttons, confirmations and live refresh.
- **Router-native.** It ships as OpenWrt packages with a LuCI page
  (Services → Kringle) for the port, public address and other settings.

The code is Rust ([hyper](https://hyper.rs) HTTP/1.1 on tokio, with
[maud](https://maud.lambda.xyz) templates), all in `src/main.rs`. The design
canvas it was built from is in `design/`.

## Install on the router

Kringle has a project site with screenshots and step-by-step setup:
**https://oceaneilonwy.github.io/kringle**. It also hosts Kringle's signed
package repository for OpenWrt 25.12 on `aarch64_cortex-a53` (Flint 2).

**In LuCI (no SSH needed):**

1. Go to System › Administration › *Repo Public Keys*. Paste the contents of
   [`openwrt/keys/kringle.pem`](openwrt/keys/kringle.pem) (or its URL,
   `https://oceaneilonwy.github.io/kringle/kringle.pem`) and click
   **Add key**.
2. Go to System › Software › *Configure apk*. Add this line to
   `customfeeds.list` and click **Save**:
   `https://oceaneilonwy.github.io/kringle/openwrt/25.12/aarch64_cortex-a53/packages.adb`
3. In System › Software, click **Update lists…**, filter for `kringle`, and
   install **luci-app-kringle**. It pulls in `kringle`.
4. Reload LuCI and go to Services › Kringle.

**Or over SSH:**

```sh
wget -O /etc/apk/keys/kringle.pem https://oceaneilonwy.github.io/kringle/kringle.pem
echo "https://oceaneilonwy.github.io/kringle/openwrt/25.12/aarch64_cortex-a53/packages.adb" >> /etc/apk/repositories.d/customfeeds.list
apk update
apk add luci-app-kringle        # pulls in kringle
```

Services › Kringle shows whether it's running and links to it, which is
`http://192.168.1.1:8787` by default. If it says *Not running*, tick
*Enabled* and Save & Apply.

**Or download the packages** from the
[Releases page](https://github.com/OceanEilonwy/kringle/releases). They're
signed with the same key, so once `kringle.pem` is trusted (step 1 above) they
install with System › Software › **Upload Package…** (the server first, then
the LuCI app) or `apk add ./kringle-*.apk ./luci-app-kringle-*.apk`.

To update, click **Update lists…** in System › Software and check the
*Updates* tab, or run `apk update && apk upgrade kringle luci-app-kringle`.
Settings and data are kept.

### Settings

You can change these in LuCI or in `/etc/config/kringle`. Saving restarts
the service.

| Setting                    | UCI option    | Default        |
|----------------------------|---------------|----------------|
| Enabled                    | `enabled`     | `1`            |
| Port                       | `port`        | `8787`         |
| Listen on                  | `listen`      | `all` (or `lan`) |
| Public address             | `public_url`  | empty: the address each browser used |
| Delete groups after (days) | `keep_days`   | `120`          |
| Data folder                | `data_dir`    | `/etc/kringle` |

The service runs as its own `kringle` user, created when the package is
installed, and restarts itself if it stops. It logs to the system log
(`logread -e kringle`).

The data folder is private to that user (0700, with the data file 0600).
Kringle only uses a folder that is its own: a new one, one it already owns,
or one holding nothing but its data file. If *Data folder* points anywhere
else (say `/etc`), the service refuses to start and says why in the log.

### Reaching it from outside your home

By default only people on your Wi-Fi can open Kringle. To let others in, you
**allow** its port through the router's firewall with a *Traffic Rule*.

> **Use the Traffic Rules tab, not Port Forwards.** Kringle runs on the
> router itself, so there's nothing to forward. A Port Forward only redirects
> traffic. It doesn't let it in, so phones on mobile data get "refused to
> connect".

**1. Allow the port in LuCI:** Network › Firewall › **Traffic Rules** tab ›
**Add** (below the list of rules). Fill in the *General Settings* tab of the
dialog:

| Field | Set it to |
|---|---|
| Name | `Kringle` |
| Protocol | `TCP` |
| Source zone | `wan` |
| Destination zone | `Device (input)` |
| Destination port | `8787` (or the port set in Services › Kringle) |
| Action | `accept` |

Click **Save**, then **Save & Apply** at the bottom of the page.

Or in a terminal:

```sh
uci add firewall rule
uci set firewall.@rule[-1].name=Kringle
uci set firewall.@rule[-1].src=wan
uci set firewall.@rule[-1].proto=tcp
uci set firewall.@rule[-1].dest_port=8787
uci set firewall.@rule[-1].target=ACCEPT
uci commit firewall
service firewall reload
```

**2. Only if another router or modem sits in front of the Flint 2** (your ISP's
box): on *that* device, forward TCP port 8787 to the Flint 2's address on that
network. This is the only place a port forward belongs. Skip this step if the
Flint 2 is connected straight to your internet line.

**3. Set the public address:** Services › Kringle. Set **Public address** to
the address people use from outside, with the port. For example
`http://203.0.113.7:8787` (your internet address is under Status › Overview ›
*IPv4 Upstream*), or a domain name that points at it. The invite and
personal links Kringle hands out use this address.

**If a phone on mobile data gets "refused to connect":** the Traffic Rule is
missing or not applied. Check with `nft list ruleset | grep 8787` on the
router. You should see a line ending in `accept comment "!fw4: Kringle"`. A
line with `redirect to :8787` and no `accept` means a Port Forward was added
instead of the Traffic Rule. Delete it under Network › Firewall › *Port
Forwards* and add the Traffic Rule above. A *timeout* rather than a refusal
usually means a router in front of the Flint 2 still needs step 2.

Kringle serves plain HTTP. If you want HTTPS, put your own reverse proxy in
front of it. Kringle respects `X-Forwarded-Proto` and `X-Forwarded-Host`, so
its links follow the proxy's address.

Guests on the guest Wi-Fi can't reach it by default, because the guest zone
blocks traffic to the router.

### Firmware upgrades

The package adds `/etc/config/kringle` and `/etc/kringle/` to
`/lib/upgrade/keep.d/`, so settings and data survive a sysupgrade. Packages
and the repository line don't survive one. Afterwards, repeat the
repository setup and `apk add luci-app-kringle`. To keep the repository line,
add `/etc/apk/keys/kringle.pem` and
`/etc/apk/repositories.d/customfeeds.list` to `/etc/sysupgrade.conf`.

## How the repository is built

`.github/workflows/openwrt.yml` runs on every push and pull request. It runs
the tests, builds both packages and uploads them as a build artifact
(`kringle-openwrt-25.12-aarch64_cortex-a53`, handy for testing a branch).
Pushes to `main` also deploy the signed repository to GitHub Pages.

The build is `scripts/build-openwrt.sh`, which you can also run locally
(needs rustup and Docker):

```sh
scripts/build-openwrt.sh                        # -> dist/*.apk and site/
```

1. It cross-compiles the binary with cargo on the latest nightly. It installs
   that toolchain itself; set `NIGHTLY=nightly-YYYY-MM-DD` to pick one. The
   standard library is rebuilt without panic messages, Debug formatting or
   unwind tables (`-Zbuild-std`, `panic = immediate-abort`), which roughly
   halves the binary to about 0.5 MB. Tests and everyday development use
   stable. Everything in the dependency tree is pure Rust, so rust-lld and
   the musl files in the rustup target are enough (see `.cargo/config.toml`).
2. The official OpenWrt SDK (`openwrt/sdk:mediatek-filogic-25.12.5`, in
   Docker) packages it with the LuCI app.
3. The SDK writes a `packages.adb` index, signed with the repository key.

Its output:

- `kringle-*.apk` contains the server, its procd service, the settings file
  `/etc/config/kringle`, and a rule that keeps the settings and data across
  firmware upgrades.
- `luci-app-kringle-*.apk` contains the Services → Kringle page in LuCI.
- `site/` contains the project site. That's the landing page from
  `web/index.html` with the screenshots in `web/screenshots/`, the repository
  under `openwrt/25.12/aarch64_cortex-a53/`, and the public key.

The screenshots are taken from the real app. `scripts/screenshots.sh` runs
it with a demo group and captures the pages with headless Chromium. It needs
ImageMagick for WebP. Re-run it after UI changes and commit the new images.

**Releases.** When a push to `main` carries a new version in `Cargo.toml`,
the workflow also publishes a GitHub release (`v0.1.1`, …) with both packages
and the key attached (`scripts/release.sh`). Existing releases are never
changed. Bump the version to cut a new one, and bump `PKG_VERSION` in
`openwrt/kringle/Makefile` with it.

**Signing key.** Routers trust `openwrt/keys/kringle.pem`. The SDK signs only
the repository index, so the build also signs each `.apk` with
`apk adbsign`. That's what lets a downloaded package install directly. The matching
private key is the `APK_SIGNING_KEY` repository secret, and a local copy
lives at `~/.config/kringle/apk-signing-key.pem`. The script refuses a key
that doesn't match the committed public key.

Pull requests can't read the secret, so their builds sign with a throwaway
key and don't deploy. Install their artifacts with
`apk add --allow-untrusted`.

If the key is ever lost or leaked, make a new one and commit its public
half. Then update the secret, and have every router fetch `kringle.pem`
again.

The package recipes in `openwrt/` also work as an OpenWrt feed
(`src-link kringle /path/to/openwrt`). Put the cross-compiled binary at
`openwrt/kringle/files/kringle` first.

## Command line

The binary also runs on its own:

| Flag           | Env                  | Default                    |
|----------------|----------------------|----------------------------|
| `--addr`       | `KRINGLE_ADDR`       | `0.0.0.0:8787`             |
| `--data`       | `KRINGLE_DATA`       | `./kringle.json`           |
| `--public-url` | `KRINGLE_PUBLIC_URL` | the host the browser used  |
| `--keep-days`  | `KRINGLE_KEEP_DAYS`  | `120`                      |

Groups are deleted `--keep-days` after they were created. Groups nobody but
the host joined are deleted after a week. Hosts can also delete their group
from the bottom of the admin page. Clean-up runs at start-up and hourly.

Limits, to keep a small router responsive even with the port open to the
internet:

- Each address can start 5 groups, then one more every 12 minutes. The server
  holds 1000 groups at most.
- 60 people per group. Names can be up to 40 characters, group names 60 and
  wishes 600.
- Forms are capped at 16 KB and must arrive within 10 seconds, as must request
  headers.
- At most 128 connections are served at once.

## Privacy

The host can't see pairs through the app. Anyone with root on the router can
read `/etc/kringle/kringle.json`, which includes the draw, so whoever runs the
router shouldn't be the one who peeks.

## Develop

```sh
cargo run -- --addr 127.0.0.1:8787   # http://127.0.0.1:8787
cargo test                           # every page and action, plus the draw logic
```

`static/town.svg` and `static/clouds.svg` are served from pre-gzipped
copies. After editing one, run `gzip -9nkf static/town.svg` (or
`clouds.svg`). A test fails if a `.gz` file is out of date.

Fonts: IM Fell English, by Igino Marini, under the SIL Open Font License
(`static/fonts/OFL.txt`).
