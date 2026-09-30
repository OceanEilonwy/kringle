#!/bin/sh
# Publish dist/*.apk as a GitHub release for the version in Cargo.toml.
#
# Runs in CI after every push to main. A version is released once: if its
# tag (v0.1.1, ...) already exists this does nothing, so bump the version in
# Cargo.toml (and openwrt/kringle/Makefile) to publish a new release.
# Needs the gh CLI with a token that can write to the repository.
#
#   scripts/release.sh            publish
#   DRY_RUN=1 scripts/release.sh  print the release notes instead
set -eu
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
tag="v$version"
repo="${GITHUB_REPOSITORY:-$(git remote get-url origin | sed -E 's#^(git@github\.com:|https://github\.com/)##; s#\.git$##')}"
owner=$(printf '%s' "${repo%%/*}" | tr '[:upper:]' '[:lower:]')
pages="${PAGES_URL:-https://$owner.github.io/${repo#*/}}"

set -- dist/*.apk
[ -e "$1" ] || { echo "no packages in dist/; run scripts/build-openwrt.sh first" >&2; exit 1; }
server=$(basename "$(ls dist/kringle-*.apk)")
luci=$(basename "$(ls dist/luci-app-kringle-*.apk)")

notes=$(mktemp)
trap 'rm -f "$notes"' EXIT
cat >"$notes" <<EOF
Kringle $version for **OpenWrt 25.12** on \`aarch64_cortex-a53\` routers (GL.iNet Flint 2 and other MediaTek Filogic devices).

**Easiest:** add the Kringle package repository and install from System › Software, so updates arrive through the router's own software page. See $pages for step-by-step instructions.

### Install these files directly

The packages are signed with the Kringle key. Trust it once, then install:

**In LuCI**
1. System › Administration › *Repo Public Keys*: paste \`$pages/kringle.pem\` and click **Add key**.
2. System › Software › **Upload Package…**: upload \`$server\` first, then \`$luci\`.
3. Reload LuCI and open Services › Kringle.

**In a terminal**
\`\`\`sh
wget -O /etc/apk/keys/kringle.pem $pages/kringle.pem
apk add ./$server ./$luci
\`\`\`

| File | What it is |
|---|---|
| \`$server\` | The Kringle server |
| \`$luci\` | Its settings page in LuCI (Services › Kringle) |
| \`kringle.pem\` | The public key the packages are signed with |
EOF

if [ -n "${DRY_RUN:-}" ]; then
	echo "== would release $tag with: $* openwrt/keys/kringle.pem"
	cat "$notes"
	exit 0
fi

if gh release view "$tag" --repo "$repo" >/dev/null 2>&1; then
	echo "$tag is already released; bump the version in Cargo.toml to publish a new one."
	exit 0
fi

gh release create "$tag" "$@" openwrt/keys/kringle.pem \
	--repo "$repo" \
	--title "Kringle $version" \
	--notes-file "$notes" \
	--target "${GITHUB_SHA:-$(git rev-parse HEAD)}"
