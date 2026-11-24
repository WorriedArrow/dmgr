#!/usr/bin/env bash
# macOS conformance tests: images written by dmgr must be accepted by Apple's
# own tools (hdiutil, fsck_hfs, Finder), and images written by hdiutil must
# be readable by dmgr.
#
# Usage: scripts/conformance.sh [path/to/dmgr] [cross-images-dir]
set -uo pipefail

DMGR=$(cd "$(dirname "${1:-target/release/dmgr}")" && pwd)/$(basename "${1:-target/release/dmgr}")
CROSS=${2:-}
HERE=$(cd "$(dirname "$0")" && pwd)
WORK=$(mktemp -d /tmp/dmgr-conformance.XXXXXX)
FIX="$WORK/fixture"
FAILURES=()
PASSES=0

log() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
pass() { PASSES=$((PASSES + 1)); printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { FAILURES+=("$*"); printf '  \033[31mFAIL\033[0m %s\n' "$*"; }

manifest() { python3 "$HERE/manifest.py" "$@" --skip .DS_Store --skip .background --skip .VolumeIcon.icns --skip .fseventsd --skip .Trashes; }

# Attach an image; prints the whole-disk device. Extra args go to hdiutil.
attach() {
  local img=$1; shift
  local out
  out=$(hdiutil attach -readonly -noautoopen "$@" "$img" 2>&1) || { echo "$out" >&2; return 1; }
  echo "$out" | awk '/^\/dev\//{print $1; exit}'
}

# Device node of the HFS volume for an image attached with -nomount.
hfs_device() {
  local img=$1 out
  out=$(hdiutil attach -readonly -nomount -noautoopen "$img" 2>&1) || { echo "$out" >&2; return 1; }
  local dev
  dev=$(echo "$out" | awk '/Apple_HFS/{print $1; exit}')
  [ -n "$dev" ] || dev=$(echo "$out" | awk '/^\/dev\//{d=$1} END{print d}')
  echo "$dev"
}

detach() { hdiutil detach "$1" -quiet 2>/dev/null || hdiutil detach "$1" -force -quiet 2>/dev/null || true; }

# fsck_hfs on the image's volume.
fsck_image() {
  local img=$1 name=$2 dev whole
  dev=$(hfs_device "$img") || { fail "$name: attach -nomount"; return; }
  whole=/dev/$(basename "$dev" | sed -E 's/(disk[0-9]+).*/\1/')
  local raw=${dev/\/dev\/disk//dev/rdisk}
  local out
  if out=$(sudo fsck_hfs -n -f "$raw" 2>&1); then
    pass "$name: fsck_hfs"
  else
    fail "$name: fsck_hfs"
    echo "$out" | sed 's/^/      /'
    sudo fsck_hfs -n -f -d "$raw" 2>&1 | tail -40 | sed 's/^/      /'
  fi
  detach "$whole"
}

# Full check of a dmgr-produced image against an expected manifest.
check_image() {
  local img=$1 name=$2 expected=$3 format=$4 manifest_flags=${5:-}
  local info
  if info=$(hdiutil imageinfo "$img" 2>&1); then
    local got
    got=$(echo "$info" | awk -F': ' '/^Format:/{print $2; exit}')
    if [ "$got" = "$format" ]; then pass "$name: imageinfo format $got"; else fail "$name: imageinfo format '$got' != $format"; fi
  else
    fail "$name: hdiutil imageinfo"; echo "$info" | sed 's/^/      /'; return
  fi
  local v
  if v=$(hdiutil verify "$img" 2>&1); then pass "$name: hdiutil verify"; else fail "$name: hdiutil verify"; echo "$v" | tail -5 | sed 's/^/      /'; fi

  local mnt="$WORK/mnt-$RANDOM"
  mkdir -p "$mnt"
  local dev
  if dev=$(attach "$img" -nobrowse -mountpoint "$mnt"); then
    pass "$name: attach"
    # shellcheck disable=SC2086
    if diff -u "$expected" <(manifest "$mnt" $manifest_flags) > "$WORK/diff.txt"; then
      pass "$name: contents match"
    else
      fail "$name: contents differ"; head -40 "$WORK/diff.txt" | sed 's/^/      /'
    fi
    detach "$dev"
  else
    fail "$name: attach"
  fi
  fsck_image "$img" "$name"
}

log "Environment"
sw_vers
"$DMGR" --version

log "Building fixture"
python3 "$HERE/make_fixture.py" "$FIX" --big
# macOS-only metadata: extended attributes, a resource fork and Finder info.
xattr -w com.example.note "hello from xattr" "$FIX/README.txt"
xattr -w com.example.dirnote "folder attr" "$FIX/with spaces"
python3 - "$FIX/README.txt" <<'EOF'
import os, sys
with open(sys.argv[1] + "/..namedfork/rsrc", "wb") as f:
    f.write(b"resource fork payload" * 100)
EOF
xattr -wx com.apple.FinderInfo "54455854747478740000000000000000 00000000000000000000000000000000" "$FIX/README.txt"
# A large extended attribute (stored out of line in the attributes B-tree).
xattr -w com.example.big "$(python3 -c 'print("x"*20000)')" "$FIX/Ünïcödé/café.txt"
manifest "$FIX" > "$WORK/fixture.manifest"
wc -l "$WORK/fixture.manifest"

log "dmgr images: every format, file system and layout"
for format in UDZO ULFO ULMO UDBZ UDRO UDRW; do
  for fs in hfs+ hfsx; do
    for layout in gpt none; do
      name="$format/$fs/$layout"
      img="$WORK/$format-$fs-$layout.dmg"
      if "$DMGR" create -q -o "$img" -V "Conformance $format" -f "$format" --fs "$fs" --layout "$layout" -s "$FIX"; then
        pass "$name: dmgr create ($(du -h "$img" | cut -f1))"
        check_image "$img" "$name" "$WORK/fixture.manifest" "$format"
        if "$DMGR" verify -q "$img"; then pass "$name: dmgr verify"; else fail "$name: dmgr verify"; fi
      else
        fail "$name: dmgr create"
      fi
    done
  done
done

log "Extended attributes, resource forks and Finder info survive"
img="$WORK/UDZO-hfs+-gpt.dmg"
mnt="$WORK/mnt-xattr"; mkdir -p "$mnt"
if dev=$(attach "$img" -nobrowse -mountpoint "$mnt"); then
  [ "$(xattr -p com.example.note "$mnt/README.txt" 2>/dev/null)" = "hello from xattr" ] && pass "inline xattr" || fail "inline xattr"
  [ "$(xattr -p com.example.dirnote "$mnt/with spaces" 2>/dev/null)" = "folder attr" ] && pass "folder xattr" || fail "folder xattr"
  [ "$(xattr -p com.example.big "$mnt/Ünïcödé/café.txt" 2>/dev/null | wc -c | tr -d ' ')" = "20001" ] && pass "large xattr" || fail "large xattr"
  cmp -s "$FIX/README.txt/..namedfork/rsrc" "$mnt/README.txt/..namedfork/rsrc" && pass "resource fork" || fail "resource fork"
  [ "$(xattr -px com.apple.FinderInfo "$mnt/README.txt" 2>/dev/null | tr -d ' \n' | cut -c1-16)" = "5445585474747874" ] && pass "Finder info type/creator" || fail "Finder info type/creator"
  [ "$(stat -f %Lp "$mnt/Fixture.app/Contents/MacOS/Fixture")" = "755" ] && pass "executable mode" || fail "executable mode"
  [ "$(readlink "$mnt/link-absolute")" = "/Applications" ] && pass "absolute symlink" || fail "absolute symlink"
  detach "$dev"
else
  fail "attach for xattr checks"
fi

log "Read/write image can be mounted writable and modified"
img="$WORK/UDRW-hfs+-gpt.dmg"
mnt="$WORK/mnt-rw"; mkdir -p "$mnt"
if out=$(hdiutil attach -nobrowse -noautoopen -mountpoint "$mnt" "$img" 2>&1); then
  dev=$(echo "$out" | awk '/^\/dev\//{print $1; exit}')
  if echo "written on mac" > "$mnt/new-file.txt" && mkdir "$mnt/new-dir" && rm "$mnt/README.txt"; then
    pass "write to UDRW volume"
  else
    fail "write to UDRW volume"
  fi
  detach "$dev"
  fsck_image "$img" "UDRW after writes"
  if "$DMGR" verify -q "$img"; then pass "dmgr verify after macOS writes"; else fail "dmgr verify after macOS writes"; fi
  "$DMGR" cat "$img" new-file.txt | grep -q "written on mac" && pass "dmgr reads file written by macOS" || fail "dmgr reads file written by macOS"
else
  fail "attach UDRW read-write"; echo "$out"
fi

log "Finder layout (.DS_Store, background, volume icon)"
python3 -m pip install --quiet --break-system-packages ds_store mac_alias 2>/dev/null || python3 -m pip install --quiet ds_store mac_alias || true
cp /System/Library/CoreServices/CoreTypes.bundle/Contents/Resources/GenericApplicationIcon.icns "$WORK/vol.icns" 2>/dev/null || printf 'icns' > "$WORK/vol.icns"
sips -s format png -z 400 600 /System/Library/Desktop\ Pictures/*.heic --out "$WORK/bg.png" >/dev/null 2>&1 || python3 - "$WORK/bg.png" <<'EOF'
import struct, sys, zlib
w, h = 600, 400
raw = b"".join(b"\0" + bytes([40, 90, 160]) * w for _ in range(h))
def chunk(t, d): return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d))
png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b"")
open(sys.argv[1], "wb").write(png)
EOF
img="$WORK/layout.dmg"
if "$DMGR" create -q -o "$img" -V "Layout Test" "$FIX/Fixture.app" --applications-link \
    --icon "$WORK/vol.icns" --background "$WORK/bg.png" --window-pos 200,150 --window-size 600,400 \
    --icon-size 96 --position "Fixture.app=150,200" --position "Applications=450,200" --hide-extension Fixture.app; then
  pass "dmgr create with layout"
  fsck_image "$img" "layout"
  if dev=$(attach "$img"); then
    vol="/Volumes/Layout Test"
    python3 - "$vol" <<'EOF' && pass ".DS_Store parsed by ds_store/mac_alias" || fail ".DS_Store parsed by ds_store/mac_alias"
import sys
from ds_store import DSStore
from mac_alias import Alias
vol = sys.argv[1]
with DSStore.open(vol + "/.DS_Store", "r") as d:
    icvp = d["."]["icvp"]
    bwsp = d["."]["bwsp"]
    assert icvp["iconSize"] == 96, icvp
    assert icvp["backgroundType"] == 2, icvp
    assert bwsp["WindowBounds"] == "{{200, 150}, {600, 400}}", bwsp
    assert d["Fixture.app"]["Iloc"] == (150, 200), d["Fixture.app"]["Iloc"]
    assert d["Applications"]["Iloc"] == (450, 200)
    a = Alias.from_bytes(icvp["backgroundImageAlias"])
    print("alias:", a.volume.name, a.target.filename, a.target.cnid, a.target.folder_cnid)
    assert a.volume.name == "Layout Test"
    assert a.target.filename == "bg.png"
    import os
    assert os.stat(vol + "/.background/bg.png").st_ino == a.target.cnid
    assert os.stat(vol + "/.background").st_ino == a.target.folder_cnid
EOF
    if command -v GetFileInfo >/dev/null; then
      GetFileInfo -a "$vol" | grep -q C && pass "volume custom icon flag" || fail "volume custom icon flag"
      GetFileInfo -a "$vol/Fixture.app" | grep -q E && pass "hidden extension flag" || fail "hidden extension flag"
    fi
    # Ask Finder what it sees (informational: needs Apple Events permission).
    if osa=$(osascript -e 'tell application "Finder"
        set d to disk "Layout Test"
        open d
        delay 2
        set w to container window of d
        set b to bounds of w
        set p to position of item "Fixture.app" of d
        set s to icon size of icon view options of w
        close w
        return (b as text) & " | " & (p as text) & " | " & (s as text)
      end tell' 2>&1); then
      echo "      Finder reports: $osa"
      case "$osa" in *"| 150, 200 |"*|*"| 150200 |"*) pass "Finder icon position" ;; *) echo "      (Finder position not verified)";; esac
    else
      echo "      osascript unavailable: $osa"
    fi
    detach "$dev"
  else
    fail "attach layout image"
  fi
else
  fail "dmgr create with layout"
fi

log "dmgr reads images created by hdiutil"
small="$WORK/small-fixture"
python3 "$HERE/make_fixture.py" "$small" --many 200
manifest "$small" --no-mode | grep -v '^\.' > "$WORK/small.manifest"
for spec in "UDZO HFS+ GPTSPUD" "ULFO HFS+ GPTSPUD" "ULMO HFS+ GPTSPUD" "UDBZ HFS+ SPUD" "UDRO JHFS+ NONE" "UDRW Case-sensitive_HFS+ GPTSPUD" "UDZO JHFS+ MBRSPUD"; do
  set -- $spec
  format=$1; fs=${2//_/ }; layout=$3
  name="hdiutil $format/$fs/$layout"
  img="$WORK/hdiutil-$format-${2}-$layout.dmg"
  if out=$(hdiutil create -quiet -srcfolder "$small" -volname "From hdiutil" -fs "$fs" -layout "$layout" -format "$format" "$img" 2>&1); then
    if "$DMGR" verify -q "$img"; then pass "$name: dmgr verify"; else fail "$name: dmgr verify"; fi
    "$DMGR" info --chunks "$img" > "$WORK/info.txt" 2>&1 && pass "$name: dmgr info" || { fail "$name: dmgr info"; cat "$WORK/info.txt"; }
    grep -m3 -E "buffers needed|zlib|lzfse|lzma|bzip2" "$WORK/info.txt" | sed 's/^/      /' || true
    rm -rf "$WORK/x"
    if "$DMGR" extract "$img" "$WORK/x" 2>/dev/null; then
      if diff -u "$WORK/small.manifest" <(manifest "$WORK/x" --no-mode | grep -v '^\.') > "$WORK/diff.txt"; then
        pass "$name: dmgr extract matches"
      else
        fail "$name: dmgr extract differs"; head -20 "$WORK/diff.txt" | sed 's/^/      /'
      fi
    else
      fail "$name: dmgr extract"
    fi
    conv="$WORK/conv-$format.dmg"
    if "$DMGR" convert -y "$img" -o "$conv" -f ULFO >/dev/null 2>&1 && hdiutil verify -quiet "$conv"; then
      pass "$name: dmgr convert -> hdiutil verify"
    else
      fail "$name: dmgr convert -> hdiutil verify"
    fi
  else
    echo "      hdiutil create failed (skipped): $out"
  fi
done

log "dmgr reads HFS+ compressed files (decmpfs)"
comp="$WORK/compressed"
ditto --hfsCompression "$small" "$comp" 2>/dev/null
img="$WORK/hdiutil-compressed.dmg"
if hdiutil create -quiet -srcfolder "$comp" -format UDRW -fs HFS+ "$img" 2>/dev/null; then
  rm -rf "$WORK/x"
  if "$DMGR" extract "$img" "$WORK/x" 2>/dev/null && diff -u "$WORK/small.manifest" <(manifest "$WORK/x" --no-mode | grep -v '^\.') > "$WORK/diff.txt"; then
    pass "extract compressed files"
  else
    fail "extract compressed files"; head -20 "$WORK/diff.txt" | sed 's/^/      /'
  fi
fi

if [ -n "$CROSS" ] && [ -d "$CROSS" ]; then
  log "Images built on other platforms"
  for dir in "$CROSS"/*/; do
    platform=$(basename "$dir")
    flags=""
    [ -f "$dir/no-mode" ] && flags="--no-mode"
    for img in "$dir"/*.dmg; do
      [ -e "$img" ] || continue
      base=$(basename "$img" .dmg)
      format=${base%%-*}
      check_image "$img" "$platform/$base" "$dir/manifest.txt" "$format" "$flags"
    done
  done
fi

log "Summary"
echo "passed: $PASSES"
if [ ${#FAILURES[@]} -gt 0 ]; then
  echo "failed: ${#FAILURES[@]}"
  printf '  - %s\n' "${FAILURES[@]}"
  exit 1
fi
echo "all conformance checks passed"
