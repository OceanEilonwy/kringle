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
- **Small.** It's a static binary of about 1.3 MB with no runtime
  dependencies. The templates, CSS, fonts and artwork are compiled in. All
  state lives in one JSON file, rewritten atomically after each change.
- **Works without JavaScript.** Every action is a plain form. A few lines of
  JS add copy buttons, confirmations and live refresh.
- **Router-native.** It ships as OpenWrt packages with a LuCI page
  (Services → Kringle) for the port, public address and other settings.

The code is Rust ([axum](https://github.com/tokio-rs/axum) with
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

The service runs as `nobody` and restarts itself if it stops. It logs to the
system log (`logread -e kringle`).

### Reaching it from outside your home

By default only people on your Wi-Fi can open Kringle. To let others in,
forward its port:

1. In LuCI, go to Network → Firewall → Traffic Rules → Add. Set Protocol TCP,
   Source zone `wan`, Destination zone *Device (input)*, Destination port
   `8787`, Action *accept*. If another router or modem sits in front of the
   Flint 2, forward TCP port 8787 to it there too.
2. Set **Public address** to your internet address and port (for example
   `http://203.0.113.7:8787`, shown under Status → Overview), so the invite
   and personal links Kringle hands out work from outside.

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
(needs Rust and Docker):

```sh
rustup target add aarch64-unknown-linux-musl   # once
scripts/build-openwrt.sh                        # -> dist/*.apk and site/
```

1. It cross-compiles the binary with cargo. Everything in the dependency tree
   is pure Rust, so rust-lld and the musl files in the rustup target are
   enough (see `.cargo/config.toml`).
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

**Signing key.** Routers trust `openwrt/keys/kringle.pem`. The matching
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

The limits are 60 people per group and 1000 groups per server. Names can be
up to 40 characters, group names 60 and wishes 600.

## Privacy

The host can't see pairs through the app. Anyone with root on the router can
read `/etc/kringle/kringle.json`, which includes the draw, so whoever runs the
router shouldn't be the one who peeks.

## Develop

```sh
cargo run -- --addr 127.0.0.1:8787   # http://127.0.0.1:8787
cargo test                           # every page and action, plus the draw logic
```

Fonts: IM Fell English, by Igino Marini, under the SIL Open Font License
(`static/fonts/OFL.txt`).
