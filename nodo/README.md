# Kringle on monokulo-nodo

This folder is how Kringle reaches [monokulo-nodo](https://github.com/OceanEilonwy/monokulo-nodo)
devices: the xbps recipes for `kringle` and the tests of their files. A device installs them from the
project's signed feed, https://nodo.monokulo.com/feed.

```
nodo/<package>/template      the xbps-src recipe (a feed app: everything under /opt/nodo/apps/<package>)
nodo/<package>/files/        its manifest.toml, settings.lua, launcher and panels
nodo/tests/<crate>/*.rs      tests run against monokulo-nodo's crate of that name
```

## Releasing

Push a version tag (`v1.2.0`, which becomes package version `1.2.0`). The `nodo` job
(`.github/workflows/openwrt.yml and nodo.yml`) calls monokulo-nodo's
[`app-release.yml`](https://github.com/OceanEilonwy/monokulo-nodo/blob/main/.github/workflows/app-release.yml), which:

1. pins each recipe to the tag: `version=` from it, `_commit=` its commit, and the first
   checksum that commit's GitHub tarball. The revision stays when the version is the recipe's
   own, and is 1 otherwise;
2. refuses a version that is not newer than the one the feed publishes (devices refuse
   downgrades);
3. runs `nodo/tests` against monokulo-nodo's crates;
4. builds aarch64 and x86_64 in monokulo-nodo's builder;
5. runs every package through the device's own install review with a throwaway key;
6. attaches `<package>-<version>_<revision>.<arch>.xbps` to the tag's release;
7. starts monokulo-nodo's feed workflow, which signs and publishes them.

Step 7 needs the repository secret `NODO_DISPATCH_TOKEN`: a fine-grained token for
OceanEilonwy/monokulo-nodo with Contents read and write. Without it the feed picks the release up
on its next scheduled run, within six hours.

To release a recipe change on its own for a version already released, raise `revision=` in the
template and run the workflow by hand on that tag (Actions › nodo › Run workflow).

The rest of the template (the pinned nightly and its checksums, the build steps) is maintained
here. To build locally, copy `nodo/<package>` into a monokulo-nodo checkout's `packages/` and
run `scripts/build-packages.sh aarch64 <package>` there.
