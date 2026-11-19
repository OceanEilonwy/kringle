# Kringle on monokulo-nodo

This folder is how Kringle reaches [monokulo-nodo](https://github.com/OceanEilonwy/monokulo-nodo)
devices: the xbps recipes for `kringle`, the tests of their files, and the public key of this
repository's own feed.

## Adding the feed on a device

Sources › add, then:

| | |
|---|---|
| Address | `https://github.com/OceanEilonwy/kringle/releases/latest/download` |
| Key | `15:40:3c:e4:96:8c:06:fc:99:46:9f:a9:24:4b:8b:07` (RSA 4096, `nodo/feed-signing-key.pub.pem`) |

Confirm the key only if the device shows this fingerprint. The feed is the newest release's:
each release tag publishes the packages, so devices see a new version once it is released.

```
nodo/<package>/template          the xbps-src recipe (a feed app: everything under /opt/nodo/apps/<package>)
nodo/<package>/files/            its manifest.toml, settings.lua, launcher and panels
nodo/tests/<crate>/*.rs          tests run against monokulo-nodo's crate of that name
nodo/feed-signing-key.pub.pem    the public half of the key the feed is signed with
```

## Releasing

Push a version tag (`v1.2.0`, which becomes package version `1.2.0`). The `nodo` job
(`.github/workflows/openwrt.yml` after it tags a release, or `nodo.yml` for a tag pushed by hand) calls monokulo-nodo's
[`app-release.yml`](https://github.com/OceanEilonwy/monokulo-nodo/blob/main/.github/workflows/app-release.yml), which:

1. pins each recipe to the tag: `version=` from it, `_commit=` its commit, and the first
   checksum that commit's GitHub tarball. The revision stays when the version is the recipe's
   own, and is 1 otherwise;
2. refuses a version that is not newer than every one this repository's earlier releases carry
   (devices refuse downgrades);
3. runs `nodo/tests` against monokulo-nodo's crates;
4. builds aarch64 and x86_64 in monokulo-nodo's builder;
5. runs every package through the device's own install review;
6. signs the feed with this repository's `NODO_FEED_SIGNING_KEY` secret, checks it against
   `nodo/feed-signing-key.pub.pem`, and attaches it to the tag's release
   (`aarch64-repodata`, `x86_64-repodata`, the `.xbps` files and their `.sig2`);
7. reads the feed back from the release as a device would.

To release a recipe change on its own for a version already released, raise `revision=` in the
template and run the workflow by hand on that tag (Actions › nodo › Run workflow).

## The key

The private key is the repository secret `NODO_FEED_SIGNING_KEY`; keep a copy offline. Devices
pin the key when the feed is added, so a new key shows there as a key change the owner must
approve. To rotate it, follow monokulo-nodo's docs/FEED.md, "Rotating an app's key".

To build locally, copy `nodo/<package>` into a monokulo-nodo checkout's `packages/` and run
`scripts/build-packages.sh aarch64 <package>` there.
