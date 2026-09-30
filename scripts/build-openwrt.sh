#!/bin/sh
# Build the kringle and luci-app-kringle packages (.apk) for OpenWrt 25.12 on
# the GL.iNet Flint 2 (mediatek/filogic, aarch64_cortex-a53), plus a signed
# apk repository ready for GitHub Pages.
#
# The Rust binary is cross-compiled here with cargo; the official OpenWrt SDK
# (in Docker) then packages it and the LuCI app and signs the repository index.
#
# Output:
#   dist/*.apk   the two packages, for installing by hand
#   site/        the package repository and a page explaining how to add it
#
# Signing key (must match openwrt/keys/kringle.pem), first found wins:
#   $APK_SIGNING_KEY       the PEM itself (how CI passes the secret)
#   $KRINGLE_SIGNING_KEY   path to the PEM
#   ~/.config/kringle/apk-signing-key.pem
# Without one the index is signed with a throwaway key: fine for testing, but
# routers that trust kringle.pem will refuse it.
set -eu
cd "$(dirname "$0")/.."

SDK_IMAGE="${SDK_IMAGE:-openwrt/sdk:mediatek-filogic-25.12.5}"
RELEASE=25.12
ARCH=aarch64_cortex-a53
PUBKEY=openwrt/keys/kringle.pem

# Where the repository is published, e.g. https://owner.github.io/kringle
remote=$(git remote get-url origin 2>/dev/null || true)
slug=$(printf '%s' "$remote" | sed -E 's#^(git@github\.com:|https://github\.com/)##; s#\.git$##')
REPO_URL="${REPO_URL:-https://github.com/$slug}"
if [ -z "${PAGES_URL:-}" ]; then
	owner=$(printf '%s' "${slug%%/*}" | tr '[:upper:]' '[:lower:]')
	PAGES_URL="https://$owner.github.io/${slug#*/}"
fi

key="${APK_SIGNING_KEY:-}"
if [ -z "$key" ]; then
	keyfile="${KRINGLE_SIGNING_KEY:-$HOME/.config/kringle/apk-signing-key.pem}"
	[ -f "$keyfile" ] && key=$(cat "$keyfile")
fi
if [ -n "$key" ]; then
	if ! printf '%s\n' "$key" | openssl ec -pubout 2>/dev/null | cmp -s - "$PUBKEY"; then
		echo "error: the signing key doesn't match $PUBKEY" >&2
		exit 1
	fi
else
	echo "warning: no signing key, so the repository index is signed with a throwaway key" >&2
fi

cargo build --release --locked --target aarch64-unknown-linux-musl
install -m 0755 target/aarch64-unknown-linux-musl/release/kringle openwrt/kringle/files/kringle

rm -rf dist site
mkdir -p dist/repo
chmod 0777 dist dist/repo # the SDK runs as its own user

# The SDK's working tree lives in a Docker volume so feeds and host tools are
# fetched and built once, not on every run. SDK_VOLUME= (empty) for a clean,
# throwaway SDK; CI does that since each job starts fresh anyway.
tag=${SDK_IMAGE##*:}
SDK_VOLUME="${SDK_VOLUME-kringle-sdk-$tag}"
set -- -v "$PWD/openwrt:/feed:ro" -v "$PWD/dist:/dist"
[ -n "$SDK_VOLUME" ] && set -- "$@" -v "$SDK_VOLUME:/builder"

docker run --rm -e APK_KEY="$key" "$@" "$SDK_IMAGE" sh -euc '
	[ -x ./setup.sh ] && [ ! -f rules.mk ] && ./setup.sh

	rm -f private-key.pem public-key.pem
	if [ -n "$APK_KEY" ]; then
		(umask 077 && printf "%s\n" "$APK_KEY" > private-key.pem)
		openssl ec -in private-key.pem -pubout -out public-key.pem 2>/dev/null
	fi

	grep -q "^src-link kringle " feeds.conf.default || echo "src-link kringle /feed" >> feeds.conf.default
	# Only the feeds the build needs; git.openwrt.org can drop connections, so retry.
	for feed in base luci kringle; do
		for try in 1 2 3; do
			./scripts/feeds update "$feed" && [ -d "feeds/$feed" ] && break
			[ "$try" = 3 ] && { echo "feed $feed failed to download" >&2; exit 1; }
			sleep 10
		done
	done
	./scripts/feeds install kringle luci-app-kringle
	make defconfig
	# Rebuild ours from scratch each time; drop earlier versions from the repository.
	rm -rf bin/packages/*/kringle build_dir/target-*/kringle-* build_dir/target-*/luci-app-kringle*
	make package/kringle/compile package/luci-app-kringle/compile -j"$(nproc)"
	make package/index
	cp bin/packages/*/kringle/* /dist/repo/
'

cp dist/repo/*.apk dist/

# The project site: landing page, assets and the package repository.
repo_path="openwrt/$RELEASE/$ARCH"
repo="site/$repo_path"
mkdir -p "$repo" site/assets/screenshots
cp dist/repo/* "$repo/"
cp "$PUBKEY" site/kringle.pem
cp web/screenshots/*.webp site/assets/screenshots/
cp web/gift.svg static/town.svg static/clouds.svg static/fonts/fell.woff2 static/fonts/fell-italic.woff2 site/assets/
files=$(for f in "$repo"/*.apk; do
	name=$(basename "$f")
	case "$name" in
		luci-app-*) what="the LuCI settings page" ;;
		*) what="the Kringle server" ;;
	esac
	printf '        <li><a href="%s/%s">%s</a>: %s</li>\n' "$repo_path" "$name" "$name" "$what"
done)
awk -v files="$files" '$0 == "@FILES@" { printf "%s\n", files; next } { print }' web/index.html |
	sed -e "s#@PAGES_URL@#$PAGES_URL#g" -e "s#@REPO_PATH@#$repo_path#g" -e "s#@REPO_URL@#$REPO_URL#g" > site/index.html
touch site/.nojekyll

ls -l dist "$repo"
