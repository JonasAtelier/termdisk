#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-only
# Rewrites THIRD-PARTY-LICENSES from its "Crates in the release binaries" line to the end: each
# crate built into the release binaries, with its licence text. Run it after any change to
# Cargo.lock and commit what it changed (AGENTS.md, "Build and test").
# Usage: scripts/notices.sh   (needs cargo and jq; the crates' sources come with `cargo fetch`)
#
# The crates are `cargo tree`'s normal dependencies on both release targets, without proc
# macros: those run while building and leave nothing in the binary. Each one's text is the
# licence file in its own source. Of the licences a crate offers, the one asking least of a
# binary: MIT, then BSL-1.0; a crate with neither gets every licence file it has. A crate
# without the file stops the script, for a person to look at.
set -euo pipefail
export LC_ALL=C # the same order in any locale
cd "$(dirname "$0")/.."
file=THIRD-PARTY-LICENSES
head='Crates in the release binaries'
line=-------------------------------------------------------------------------------
grep -qx "$head" "$file" || { echo "$file has no '$head' line" >&2; exit 1; }
trap 'rm -f "$file.new"' EXIT

# name, version, licence, source folder: one line per package in Cargo.lock.
packages=$(cargo metadata --locked --format-version 1 |
  jq -r '.packages[] | [.name, .version, .license // "", (.manifest_path | rtrimstr("/Cargo.toml"))] | @tsv')
crates=$(for target in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do
  cargo tree --locked -e normal,no-proc-macro --target "$target" --prefix none --format '{p}'
done | sed 's/ (\*)$//' | grep -v '^termdisk ' | sort -u)

{
  sed "/^$head\$/,\$d" "$file"
  echo "$head"
  echo
  echo "Each with the licence it is taken under, of those it offers, and that licence's text."
  echo "scripts/notices.sh writes this list from Cargo.lock."
  while read -r name version; do
    version=${version#v}
    IFS=$'\t' read -r licence dir < <(awk -F'\t' -v n="$name" -v v="$version" \
      '$1 == n && $2 == v { print $3 "\t" $4 }' <<< "$packages")
    files=()
    for f in "$dir"/LICEN[CS]E* "$dir"/COPYING*; do
      if [ -f "$f" ]; then files+=("$f"); fi
    done
    case " $(tr '/()' '   ' <<< "$licence") " in
      *" MIT "*) taken=MIT; text='Permission is hereby granted' ;;
      *" BSL-1.0 "*) taken=BSL-1.0; text='Boost Software License' ;;
      *) taken=$licence; text= ;;
    esac
    if [ -n "$text" ] && [ ${#files[@]} -gt 0 ]; then
      mapfile -t files < <(grep -l -- "$text" "${files[@]}" | head -1)
    fi
    if [ ${#files[@]} -eq 0 ]; then
      echo "$name $version ($licence): no $taken licence file in $dir" >&2
      exit 1
    fi
    echo
    echo "$line"
    echo "$name $version ($taken)"
    echo "https://crates.io/crates/$name"
    for f in "${files[@]}"; do
      echo
      awk '{ sub(/\r$/, ""); print }' "$f"
    done
  done <<< "$crates"
} > "$file.new"
mv "$file.new" "$file"
echo "$file: $(wc -l <<< "$crates") crates"
