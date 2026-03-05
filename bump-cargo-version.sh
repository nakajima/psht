#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "Usage: $0 [major|minor|patch|set X.Y.Z] [path/to/Cargo.toml]" >&2
  exit 1
}

mode="${1:-patch}"
manifest="Cargo.toml"
new_version=""

case "$mode" in
  major|minor|patch)
    shift || true
    ;;
  set)
    [[ $# -ge 2 ]] || usage
    new_version="$2"
    shift 2
    ;;
  *)
    usage
    ;;
esac

if [[ $# -ge 1 ]]; then
  manifest="$1"
fi

[[ -f "$manifest" ]] || { echo "Manifest not found: $manifest" >&2; exit 1; }

extract_version() {
  local section="$1"
  awk -v section="$section" '
    {
      line=$0
      sub(/[[:space:]]*#.*$/, "", line)
      gsub(/^[[:space:]]+|[[:space:]]+$/, "", line)

      if (line == "[" section "]") { in_section=1; next }
      if (in_section && line ~ /^\[/) { in_section=0 }

      if (in_section && line ~ /^version[[:space:]]*=/) {
        if (match(line, /"[0-9]+\.[0-9]+\.[0-9]+"/)) {
          print substr(line, RSTART + 1, RLENGTH - 2)
          exit
        }
      }
    }
  ' "$manifest"
}

target_section="workspace.package"
current_version="$(extract_version "$target_section")"
if [[ -z "$current_version" ]]; then
  target_section="package"
  current_version="$(extract_version "$target_section")"
fi

[[ -n "$current_version" ]] || {
  echo "Could not find version in [workspace.package] or [package]" >&2
  exit 1
}

if [[ -z "$new_version" ]]; then
  [[ "$current_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
    echo "Current version is not plain semver (X.Y.Z): $current_version" >&2
    exit 1
  }

  IFS='.' read -r major minor patch <<< "$current_version"
  case "$mode" in
    major) ((major += 1)); minor=0; patch=0 ;;
    minor) ((minor += 1)); patch=0 ;;
    patch) ((patch += 1)) ;;
  esac
  new_version="${major}.${minor}.${patch}"
fi

[[ "$new_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
  echo "New version must be X.Y.Z: $new_version" >&2
  exit 1
}

tmp_file="$(mktemp)"
awk -v section="$target_section" -v version="$new_version" '
  BEGIN { in_section=0; done=0 }
  {
    raw=$0
    line=$0
    sub(/[[:space:]]*#.*$/, "", line)
    gsub(/^[[:space:]]+|[[:space:]]+$/, "", line)

    if (line == "[" section "]") { in_section=1; print raw; next }
    if (in_section && line ~ /^\[/) { in_section=0 }

    if (in_section && !done && line ~ /^version[[:space:]]*=/) {
      match(raw, /^[[:space:]]*/)
      indent = substr(raw, RSTART, RLENGTH)
      print indent "version = \"" version "\""
      done=1
      next
    }

    print raw
  }
  END { if (!done) exit 2 }
' "$manifest" > "$tmp_file" || {
  rm -f "$tmp_file"
  echo "Failed to update version in section [$target_section]" >&2
  exit 1
}

mv "$tmp_file" "$manifest"
echo "Bumped $manifest [$target_section]: $current_version -> $new_version"
