#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-only
# Speed and memory of a scan, against du and ncdu, to compare a change with what came before.
# Usage: scripts/bench.sh [BINARY] [DIR]   (defaults: target/release/termdisk, $HOME;
#        build it first with `cargo build -j4 --release`)
#
# Medians of 5 warm-cache runs, after one untimed run (dropping the page cache needs root):
#   du -sx                 the walk alone, no tree kept
#   ncdu -x -o /dev/null   ncdu's scan, exported as it goes
#   ncdu -rr -x (pty)      ncdu in a 120x40 pty, sent `q` once its browser shows: its tree in
#                          memory, the row to compare max RSS with
#   termdisk --summary     the scan into termdisk's tree, totals printed
# Then termdisk's totals against `du -sxB1` (disk usage; uutils du 0.10 leaves mount points
# out, termdisk and find count their own size) and against `find -xdev` (apparent size,
# folders included; uutils du counts folders as 0 with -b), and
# bytes per entry: the tree's own count and (max RSS - max RSS on an empty folder) / entries.
# Then the view (`--read-only` on DIR, in scripts/smoke.py's pty): launch to the first frame
# of the scanned tree, max RSS, threads after the scan, wake-ups over 5 idle seconds; frame
# times (target/release/examples/frame: treemap layout and full draw, 120x40 and 300x80); and
# the trash of a 10 000-file folder in smoke.py's temporary home, confirm to updated screen.
set -u
cd "$(dirname "$0")/.." || exit 1
BIN=${1:-target/release/termdisk}
DIR=${2:-$HOME}
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT

# One run under /usr/bin/time: prints "wall_s rss_kb"; the command's output goes to $T/out.
timed() {
  /usr/bin/time -f '%e %M' -o "$T/time" "$@" >"$T/out" 2>/dev/null
  tail -1 "$T/time"
}

# ncdu in a pty, `q` once "Total disk usage" shows: prints "wall_s rss_kb".
pty_ncdu() {
  python3 -I - "$@" <<'EOF'
import fcntl, os, pty, select, struct, sys, termios, time
t = time.monotonic()
pid, fd = pty.fork()
if pid == 0:
    fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 120, 0, 0))
    os.environ['TERM'] = 'xterm-256color'
    os.execvp(sys.argv[1], sys.argv[1:])
tail, wall = b'', None
while select.select([fd], [], [], 120)[0]:
    try:
        b = os.read(fd, 65536)
    except OSError:
        break
    if not b:
        break
    tail = (tail + b)[-256:]
    if wall is None and b'Total disk usage' in tail:
        wall = time.monotonic() - t
        os.write(fd, b'q')
if wall is None:
    os.kill(pid, 9)
ru = os.wait4(pid, 0)[2]
print(f'{wall:.2f} {ru.ru_maxrss}' if wall is not None else 'fail fail')
EOF
}

med() { sort -g | sed -n 3p; }

# row NAME COMMAND...: five runs, one table row; leaves the runs in $T/runs.
row() {
  local name=$1
  shift
  : >"$T/runs"
  for _ in 1 2 3 4 5; do "$@" >>"$T/runs"; done
  if grep -q fail "$T/runs"; then echo "| $name | fail | |"; return; fi
  echo "| $name | $(cut -d' ' -f1 "$T/runs" | med) | $((($(cut -d' ' -f2 "$T/runs" | med) + 512) / 1024)) |"
}

echo "dir: $DIR   binary: $BIN   du: $(du --version | head -1)"
free -m | awk '/^Mem:/ { print "MemAvailable before: " $7 " MB" }'
du -sx "$DIR" >/dev/null 2>&1
echo
echo "| command | wall s | max RSS MB |"
echo "|---|---|---|"
row "du -sx" timed du -sx "$DIR"
row "ncdu -x -o /dev/null" timed ncdu -x -0 -o /dev/null "$DIR"
row "ncdu -rr -x (pty)" pty_ncdu ncdu -rr -x "$DIR"
row "termdisk --summary" timed "$BIN" --summary "$DIR"
rss=$(cut -d' ' -f2 "$T/runs" | med)
cp "$T/out" "$T/summary"

num() { sed -n "s/$1/\\1/p" "$T/summary"; }
alloc=$(num '^disk usage .*(\([0-9]*\) bytes)$')
apparent=$(num '^apparent .*(\([0-9]*\) bytes)$')
entries=$(num '^tree *\([0-9]*\) entries.*')
per_entry=$(num '.* \([0-9.]*\) B\/entry$')
du_alloc=$(du -sxB1 "$DIR" 2>/dev/null | cut -f1)
read -r find_alloc find_apparent < <(find "$DIR" -xdev -printf '%i %n %y %s %b\n' 2>/dev/null |
  awk '!($3 != "d" && $2 > 1 && seen[$1]++) { s += $4; b += $5 * 512 } END { printf "%d %d", b, s }')
mkdir "$T/empty"
base=$(timed "$BIN" --summary "$T/empty" | cut -d' ' -f2)
echo
echo "disk usage:   termdisk $alloc, find -xdev $find_alloc, difference $((alloc - find_alloc)); du -sxB1 $du_alloc, difference $((alloc - du_alloc))"
echo "apparent:     termdisk $apparent, find -xdev $find_apparent, difference $((apparent - find_apparent))"
echo "B per entry:  tree $per_entry, RSS $(((rss - base) * 1024 / entries)) ($entries entries, empty-folder RSS $base kB)"

if ! "$BIN" --help | grep -q -- --read-only; then
  exit 0 # a binary from before the view: the scan is all it has
fi
echo
echo "view on $DIR (--read-only):"
# Half a second before counting: the first frame may still be on its way to the pty.
python3 -I scripts/smoke.py "$BIN" 'waitfor:Modified,sleep:0.5,threads,ctx:5,key:q' --read-only "$DIR" |
  grep -E '^(waitfor|threads|wake-ups|max RSS|exit|restored)' | sed 's/^waitfor .Modified.:/launch to first frame:/'
FRAME=target/release/examples/frame
if [ -x "$FRAME" ]; then
  echo
  "$FRAME" "$DIR"
fi
echo
echo "trash of a 10 000-file folder (temporary home):"
python3 -I scripts/smoke.py "$BIN" \
  $'waitfor:Modified,key:\x1b[3~,sleep:0.7,key:\t,sleep:0.3,mark,key:\r,waitout,sleep:0.3,key:q' |
  grep -E '^(first output|trash files|exit)' | sed 's/^first output after the mark:/confirm to updated screen:/'
