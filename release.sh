#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
RELEASOR_BIN="${RELEASOR_BIN:-$HOME/.local/bin/releasor2000}"
CONFIG_PATH="${RELEASOR_CONFIG:-$SCRIPT_DIR/releasor2000.toml}"

err() {
  echo "error: $*" >&2
  exit 1
}

read_toml_string() {
  local section="$1"
  local key="$2"
  local file="$3"
  awk -v section="$section" -v key="$key" '
    /^\s*\[/ {
      in_section = ($0 == "[" section "]")
      next
    }
    in_section && $0 ~ "^[[:space:]]*" key "[[:space:]]*=" {
      if (match($0, /"[^"]*"/)) {
        value = substr($0, RSTART + 1, RLENGTH - 2)
        print value
        exit
      }
    }
  ' "$file"
}

read_toml_array() {
  local section="$1"
  local key="$2"
  local file="$3"
  awk -v section="$section" -v key="$key" '
    function emit_quoted(line,   m) {
      while (match(line, /"[^"]+"/)) {
        print substr(line, RSTART + 1, RLENGTH - 2)
        line = substr(line, RSTART + RLENGTH)
      }
    }
    /^\s*\[/ {
      in_section = ($0 == "[" section "]")
      in_array = 0
      next
    }
    !in_section { next }
    in_array {
      emit_quoted($0)
      if ($0 ~ /\]/) {
        in_array = 0
      }
      next
    }
    $0 ~ "^[[:space:]]*" key "[[:space:]]*=" {
      emit_quoted($0)
      if ($0 !~ /\]/) {
        in_array = 1
      }
    }
  ' "$file"
}

parse_release_args() {
  SCRIPT_VERSION=""
  FORWARDED_ARGS=()
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --version)
        shift
        [[ $# -gt 0 ]] || err "--version requires a value"
        SCRIPT_VERSION="$1"
        ;;
      --version=*)
        SCRIPT_VERSION="${1#*=}"
        ;;
      *)
        FORWARDED_ARGS+=("$1")
        ;;
    esac
    shift || true
  done
}

manifest_version() {
  awk -F'"' '/^version[[:space:]]*=/ { print $2; exit }' "$SCRIPT_DIR/Cargo.toml"
}

json_release_id() {
  if command -v jq >/dev/null 2>&1; then
    jq -r '.id // empty'
    return
  fi
  sed -n 's/.*"id":[[:space:]]*\([0-9][0-9]*\).*/\1/p' | head -n1
}

get_release_by_tag() {
  local api_base="$1"
  local token="$2"
  local tag="$3"
  curl -fsS \
    -H "Authorization: token $token" \
    "$api_base/releases/tags/$tag" 2>/dev/null || true
}

delete_release_if_exists() {
  local api_base="$1"
  local token="$2"
  local version="$3"

  local release_json=""
  local found_tag=""
  for candidate_tag in "$version" "v$version"; do
    release_json="$(get_release_by_tag "$api_base" "$token" "$candidate_tag")"
    if [[ -n "$release_json" ]]; then
      found_tag="$candidate_tag"
      break
    fi
  done

  [[ -n "$release_json" ]] || err "release $version already exists but could not fetch release by tag"
  local release_id
  release_id="$(echo "$release_json" | json_release_id)"
  [[ -n "$release_id" ]] || err "failed to parse release id for tag $found_tag"

  echo "-----> deleting existing release tag=$found_tag id=$release_id" >&2
  curl -fsS -X DELETE \
    -H "Authorization: token $token" \
    "$api_base/releases/$release_id" >/dev/null
}

get_release_id() {
  local api_base="$1"
  local token="$2"
  local version="$3"
  local release_json=""
  for candidate_tag in "$version" "v$version"; do
    release_json="$(get_release_by_tag "$api_base" "$token" "$candidate_tag")"
    if [[ -n "$release_json" ]]; then
      local release_id
      release_id="$(echo "$release_json" | json_release_id)"
      if [[ -n "$release_id" ]]; then
        echo "$release_id"
        return 0
      fi
    fi
  done
  return 1
}

wait_for_release_id() {
  local api_base="$1"
  local token="$2"
  local version="$3"
  local attempts="${4:-10}"
  local delay_seconds="${5:-1}"
  local release_id=""

  for ((i = 1; i <= attempts; i++)); do
    release_id="$(get_release_id "$api_base" "$token" "$version")" || release_id=""
    if [[ -n "$release_id" ]]; then
      echo "$release_id"
      return 0
    fi
    if (( i < attempts )); then
      sleep "$delay_seconds"
    fi
  done
  return 1
}

upload_asset() {
  local api_base="$1"
  local token="$2"
  local release_id="$3"
  local file="$4"
  local name="$5"
  local resp_file="$6"

  local status
  status="$(curl -sS -o "$resp_file" -w "%{http_code}" \
    -H "Authorization: token $token" \
    -H "Content-Type: application/octet-stream" \
    --data-binary @"$file" \
    "$api_base/releases/$release_id/assets?name=$name")" || return 1

  case "$status" in
    200|201)
      return 0
      ;;
    409)
      # Asset already exists: treat as success so reruns are idempotent.
      return 0
      ;;
    *)
      echo "upload failed for $name (HTTP $status): $(cat "$resp_file")" >&2
      return 1
      ;;
  esac
}

parse_succeeded_targets() {
  local log_file="$1"
  sed -n 's/.*Succeeded:[[:space:]]*\([A-Za-z0-9._-]\+\).*/\1/p' "$log_file" | awk '!seen[$0]++'
}

upload_cli_assets() {
  local api_base="$1"
  local token="$2"
  local version="$3"
  shift 3

  local release_id
  release_id="$(wait_for_release_id "$api_base" "$token" "$version" 12 1)" || {
    err "release $version not found after releasor run (tried tags: $version, v$version via $api_base)"
  }

  local targets=()
  if [[ "$#" -gt 0 ]]; then
    targets=("$@")
  else
    mapfile -t targets < <(read_toml_array "build" "targets" "$CONFIG_PATH")
  fi
  [[ "${#targets[@]}" -gt 0 ]] || err "no build.targets found in $CONFIG_PATH"

  local tmpdir
  tmpdir="$(mktemp -d)"
  local uploaded=0
  local failed=0

  for target in "${targets[@]}"; do
    echo "-----> building psht for $target"
    if ! cargo build --release --bin psht --target "$target"; then
      echo "-----> failed to build psht for $target (skipping)" >&2
      failed=$((failed + 1))
      continue
    fi

    local bin_path="$SCRIPT_DIR/target/$target/release/psht"
    if [[ ! -f "$bin_path" ]]; then
      echo "-----> missing built binary: $bin_path" >&2
      failed=$((failed + 1))
      continue
    fi

    local asset_name="psht-${version}-${target}.tar.gz"
    local tarball="$tmpdir/$asset_name"
    tar -C "$(dirname "$bin_path")" -czf "$tarball" "$(basename "$bin_path")"

    if upload_asset "$api_base" "$token" "$release_id" "$tarball" "$asset_name" "$tmpdir/upload.json"; then
      echo "-----> uploaded $asset_name"
      uploaded=$((uploaded + 1))
    else
      failed=$((failed + 1))
    fi
  done

  rm -rf "$tmpdir"
  if [[ "$uploaded" -eq 0 ]]; then
    err "no psht assets were uploaded"
  fi
  if [[ "$failed" -gt 0 ]]; then
    echo "-----> warning: psht upload failed for $failed target(s)" >&2
  fi
}

run_releasor() {
  local log_file="$1"
  shift
  (
    set +e
    "$RELEASOR_BIN" -c "$CONFIG_PATH" release "$@" 2>&1 | tee "$log_file"
    printf "%s" "${PIPESTATUS[0]}" >"${log_file}.status"
  )
  local status
  status="$(cat "${log_file}.status")"
  rm -f "${log_file}.status"
  return "$status"
}

[[ -x "$RELEASOR_BIN" ]] || err "releasor binary not executable: $RELEASOR_BIN"
[[ -f "$CONFIG_PATH" ]] || err "config not found: $CONFIG_PATH"

repo="$(read_toml_string "project" "repo" "$CONFIG_PATH")"
[[ -n "$repo" ]] || err "missing [project].repo in $CONFIG_PATH"
base_url="$(read_toml_string "git" "base_url" "$CONFIG_PATH")"
[[ -n "$base_url" ]] || err "missing [git].base_url in $CONFIG_PATH"
base_url_no_scheme="${base_url#*://}"
base_host="${base_url_no_scheme%%/*}"
if [[ "$base_host" == "github.com" || "$base_host" == "www.github.com" ]]; then
  api_base="https://api.github.com/repos/$repo"
  token="${GITHUB_TOKEN:-${GITEA_TOKEN:-}}"
  [[ -n "$token" ]] || err "set GITHUB_TOKEN (or GITEA_TOKEN) for GitHub release operations"
else
  api_base="${base_url%/}/api/v1/repos/$repo"
  token="${GITEA_TOKEN:-${GITHUB_TOKEN:-}}"
  [[ -n "$token" ]] || err "set GITEA_TOKEN (or GITHUB_TOKEN) for release operations"
fi

parse_release_args "$@"
version="$SCRIPT_VERSION"
if [[ -z "$version" ]]; then
  version="$(manifest_version)"
fi
[[ -n "$version" ]] || err "unable to determine release version"

log_file="$(mktemp)"
trap 'rm -f "$log_file"' EXIT

if run_releasor "$log_file" "${FORWARDED_ARGS[@]}"; then
  mapfile -t succeeded_targets < <(parse_succeeded_targets "$log_file")
  upload_cli_assets "$api_base" "$token" "$version" "${succeeded_targets[@]}"
  exit 0
fi

if ! rg -q "returned error: 409|HTTP.*409|\\b409\\b" "$log_file"; then
  cat "$log_file" >&2
  exit 1
fi

echo "-----> release conflict for version $version; using existing release to upload CLI assets" >&2
mapfile -t succeeded_targets < <(parse_succeeded_targets "$log_file")
upload_cli_assets "$api_base" "$token" "$version" "${succeeded_targets[@]}"
exit 0
