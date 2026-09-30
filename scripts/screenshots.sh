#!/bin/sh
# Regenerate the screenshots on the project site (web/screenshots/).
#
# Runs the real app on a throwaway data file, fills it with a demo group
# (the Morgans' Christmas from the design), and captures pages with headless
# Chromium. Needs curl, chromium and ImageMagick (for WebP).
set -eu
cd "$(dirname "$0")/.."

PORT=${PORT:-8798}
B="http://127.0.0.1:$PORT"
OUT=web/screenshots
tmp=$(mktemp -d)

cargo build --release --locked
./target/release/kringle --addr "127.0.0.1:$PORT" --data "$tmp/demo.json" --public-url "http://gifts.example.com:8787" >"$tmp/log" 2>&1 &
pid=$!
trap 'kill $pid 2>/dev/null; rm -rf "$tmp"' EXIT
sleep 1

# POST a form and print where it redirects to (as a path).
go() { curl -s -o /dev/null -w '%{redirect_url}' "$@" | sed "s#^$B##"; }

admin=$(go -d "group=The Morgans’ Christmas&host=Rosa&budget=30&plays=1" "$B/groups")
invite=$(curl -s "$B$admin" | grep -o '/j/[a-z0-9-]*' | head -1)
rosa=$(curl -s "$B$admin" | grep -o '/p/[a-z2-7]*' | head -1)
go -d "wishes=Books, bath stuff, anything lemon&back=me" "$B$rosa/wishes" >/dev/null

join() { go --data-urlencode "name=$1" --data-urlencode "wishes=$2" "$B$invite"; }
join "Alex" "Hot sauce and board games" >/dev/null
join "Sam" "" >/dev/null
join "Priya" "Plants! Size M tops. No candles please." >/dev/null
tom=$(join "Tom" "Coffee beans (not decaf). Anything for the garden. I’m a size L.")
join "Nana Jo" "Jigsaw puzzles, 1000 pieces" >/dev/null

# Keep apart: Alex and Sam, Priya and Tom (partners); Nana Jo had Rosa last year.
go -d "giver=2&receiver=3&both=1" "$B$admin/rules" >/dev/null
go -d "giver=4&receiver=5&both=1" "$B$admin/rules" >/dev/null
go -d "giver=6&receiver=1" "$B$admin/rules" >/dev/null

shot() { # name url width height [scale]
	chromium --headless=new --no-sandbox --disable-gpu --hide-scrollbars \
		--force-device-scale-factor="${5:-1}" --window-size="$3,$4" \
		--screenshot="$tmp/$1.png" "$B$2" >/dev/null 2>&1
	magick "$tmp/$1.png" -quality 82 "$OUT/$1.webp"
}

mkdir -p "$OUT"
shot create / 1280 1000
shot host "$admin" 1280 1560
shot join-phone "$invite" 390 1180 2

go -X POST "$B$admin/draw" >/dev/null
shot wrapped-phone "$tom" 390 1080 2
go -X POST "$B$tom/open" >/dev/null
# Rosa opens their tag too, so the progress bar shows some movement.
go -X POST "$B$rosa/open" >/dev/null
shot tag "$tom?show=1" 1280 1300
shot drawn "$admin" 1280 900

ls -l "$OUT"
