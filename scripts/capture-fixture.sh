#!/usr/bin/env bash
# Capture a fixture set on an NFS server and stream it back as a tar on stdout.
# File and directory names in `states` are replaced with stable pseudonyms
# (d00001/f00001.dcm) before anything leaves the server; the capture aborts if
# any original name would survive.
#
#   ssh root@SERVER 'bash -s' < scripts/capture-fixture.sh | tar -C tests/fixtures/local -xf -
#   cargo run -- --fixture tests/fixtures/local
#
# tests/fixtures/local/ is gitignored: mountinfo still lists export paths and
# `hosts` lists client host names, so keep captures off shared repos.
set -euo pipefail
PORT=${PORT:-2049}
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
mkdir -p "$T/ss" "$T/clients"

for i in 1 2 3; do
  ss -tniH state established "( sport = :$PORT )" > "$T/ss/00$i.txt"
  [ $i -lt 3 ] && sleep 2
done
cp /proc/self/mountinfo "$T/mountinfo"

for d in /proc/fs/nfsd/clients/*/; do
  id=$(basename "$d")
  mkdir -p "$T/clients/$id"
  cat "$d/info" > "$T/clients/$id/info"
  cat "$d/states" > "$T/clients/$id/raw"
done

# Build one original->pseudonym map across all clients.
cat "$T"/clients/*/raw | awk '
function ps(c, kind,   e, k) {
  k = kind SUBSEP c
  if (k in M) return M[k]
  e = ""
  if (kind == "f" && match(c, /\.[A-Za-z0-9]+(\.gz)?$/)) e = substr(c, RSTART)
  N[kind]++
  M[k] = sprintf("%s%05d%s", kind, N[kind], e)
  return M[k]
}
{
  if (match($0, /filename: "([^"\\]|\\.)*"/)) {
    p = substr($0, RSTART + 11, RLENGTH - 12)
    n = split(p, a, "/"); out = ""
    for (i = 1; i <= n; i++) out = out (i > 1 ? "/" : "") ps(a[i], (i == n ? "f" : "d"))
    print p "\t" out
  }
}' > "$T/map.tsv"

for d in "$T"/clients/*/; do
  awk -F'\t' '
    NR == FNR { m["filename: \"" $1 "\""] = "filename: \"" $2 "\""; next }
    {
      if (match($0, /filename: "([^"\\]|\\.)*"/)) {
        k = substr($0, RSTART, RLENGTH)
        $0 = substr($0, 1, RSTART - 1) m[k] substr($0, RSTART + RLENGTH)
      }
      print
    }' "$T/map.tsv" "$d/raw" > "$d/states"
  rm "$d/raw"
done
rm "$T/map.tsv"

bad=$(grep -h -o 'filename: "[^"]*"' "$T"/clients/*/states \
  | grep -c -v -E '^filename: "(d[0-9]{5}/)*f[0-9]{5}[^/"]*"$' || true)
if [ "$bad" != 0 ]; then
  echo "capture-fixture: $bad filenames not obfuscated; aborting" >&2
  exit 1
fi

awk '/^[0-9]/ {print $4}' "$T/ss/001.txt" \
  | sed -E 's/:[0-9]+$//; s/^\[::ffff://; s/\]$//; s/^\[//' | sort -u \
  | while read -r ip; do
      h=$(getent hosts "$ip" | awk '{print $2}') || true
      [ -n "$h" ] && echo "$ip $h"
    done > "$T/hosts" || true

tar -C "$T" -cf - .
