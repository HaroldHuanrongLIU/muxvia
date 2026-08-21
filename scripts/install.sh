#!/bin/sh

set -eu

umask 077

program=muxvia-installer
manifest_url=https://github.com/HaroldHuanrongLIU/muxvia/releases/latest/download/muxvia-latest.json
releases_url=https://github.com/HaroldHuanrongLIU/muxvia/releases/download

fail() {
  printf '%s:%s\n' "$program" "$1" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "missing-command:$1"
}

canonical_existing_path() {
  path=$1
  case "$path" in
    /*) ;;
    *) path=$(pwd)/$path ;;
  esac
  links=0
  while [ -L "$path" ]; do
    links=$((links + 1))
    [ "$links" -le 32 ] || return 1
    target=$(readlink "$path") || return 1
    case "$target" in
      /*) path=$target ;;
      *) path=$(dirname "$path")/$target ;;
    esac
    directory=$(cd -P "$(dirname "$path")" 2>/dev/null && pwd) || return 1
    path=$directory/$(basename "$path")
  done
  directory=$(cd -P "$(dirname "$path")" 2>/dev/null && pwd) || return 1
  printf '%s/%s\n' "$directory" "$(basename "$path")"
}

ownership_for_path() {
  case "$1" in
    */Cellar/*|*/Homebrew/*|/opt/homebrew/*) printf '%s\n' homebrew ;;
    */node_modules/*|*/.npm/*) printf '%s\n' npm ;;
    *) printf '%s\n' external ;;
  esac
}

check_path_ownership() {
  existing=$(command -v muxvia 2>/dev/null || true)
  [ -n "$existing" ] || return 0
  resolved=$(canonical_existing_path "$existing" 2>/dev/null || printf '%s\n' "$existing")
  if [ -f "$launcher" ] && [ ! -L "$launcher" ]; then
    own_launcher=$(canonical_existing_path "$launcher" 2>/dev/null || true)
    [ "$resolved" = "$own_launcher" ] && return 0
  fi
  owner=$(ownership_for_path "$resolved")
  case "$owner" in
    homebrew) fail "ownership-conflict:homebrew:run-brew-upgrade-muxvia" ;;
    npm) fail "ownership-conflict:npm:run-npm-install-global-muxvia" ;;
    *) fail "ownership-conflict:external:$resolved" ;;
  esac
}

check_install_ownership() {
  if [ -L "$install_root" ] || { [ -e "$install_root" ] && [ ! -d "$install_root" ]; }; then
    fail "ownership-conflict:invalid-install-root"
  fi
  if [ -e "$owner_file" ]; then
    [ -f "$owner_file" ] && [ ! -L "$owner_file" ] || fail "ownership-conflict:invalid-owner"
    [ "$(wc -l < "$owner_file" | tr -d '[:space:]')" = 1 ] || fail "ownership-conflict:invalid-owner"
    owner=$(sed -n '1p' "$owner_file")
    case "$owner" in
      verified-download) ;;
      homebrew) fail "ownership-conflict:homebrew:run-brew-upgrade-muxvia" ;;
      npm) fail "ownership-conflict:npm:run-npm-install-global-muxvia" ;;
      *) fail "ownership-conflict:unknown" ;;
    esac
  elif [ -d "$install_root" ] && [ -n "$(find "$install_root" ! -path "$install_root" -prune -print -quit)" ]; then
    fail "ownership-conflict:unknown"
  fi
  if [ -e "$active_file" ] || [ -L "$active_file" ]; then
    [ -f "$active_file" ] && [ ! -L "$active_file" ] || fail active-version-invalid
  fi
}

detect_target() {
  operating_system=$(uname -s)
  architecture=$(uname -m)
  if [ "${MUXVIA_INSTALLER_TESTING:-0}" = 1 ]; then
    operating_system=${MUXVIA_INSTALLER_TEST_OS:-$operating_system}
    architecture=${MUXVIA_INSTALLER_TEST_ARCH:-$architecture}
  fi
  case "$operating_system:$architecture" in
    Darwin:arm64|Darwin:aarch64) target=darwin-arm64 ;;
    Darwin:x86_64) target=darwin-x64 ;;
    Linux:arm64|Linux:aarch64)
      if [ "${MUXVIA_INSTALLER_TESTING:-0}:${MUXVIA_INSTALLER_TEST_GLIBC:-0}" != 1:1 ]; then
        getconf GNU_LIBC_VERSION >/dev/null 2>&1 || fail "unsupported-target:linux-musl-arm64"
      fi
      target=linux-glibc-arm64
      ;;
    Linux:x86_64)
      if [ "${MUXVIA_INSTALLER_TESTING:-0}:${MUXVIA_INSTALLER_TEST_GLIBC:-0}" != 1:1 ]; then
        getconf GNU_LIBC_VERSION >/dev/null 2>&1 || fail "unsupported-target:linux-musl-x64"
      fi
      target=linux-glibc-x64
      ;;
    *) fail "unsupported-target:$operating_system:$architecture" ;;
  esac
}

download() {
  url=$1
  output=$2
  if [ "${MUXVIA_INSTALLER_TESTING:-0}" = 1 ]; then
    curl -fsSL --proto '=https,file' "$url" -o "$output" || fail download-failed
  else
    curl -fsSL --proto '=https' --tlsv1.2 "$url" -o "$output" || fail download-failed
  fi
}

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    fail missing-command:sha256sum-or-shasum
  fi
}

parse_public_manifest() {
  public_manifest=$1
  schema=$(sed -n 's/^[[:space:]]*"schemaVersion":[[:space:]]*\([0-9][0-9]*\)[[:space:]]*,\{0,1\}[[:space:]]*$/\1/p' "$public_manifest")
  product=$(sed -n 's/^[[:space:]]*"product":[[:space:]]*"\([^"]*\)"[[:space:]]*,\{0,1\}[[:space:]]*$/\1/p' "$public_manifest")
  release=$(sed -n 's/^[[:space:]]*"release":[[:space:]]*"\([^"]*\)"[[:space:]]*,\{0,1\}[[:space:]]*$/\1/p' "$public_manifest")
  [ "$schema" = 1 ] && [ "$product" = muxvia ] || fail release-metadata-invalid
  printf '%s\n' "$release" | grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' \
    || fail release-metadata-invalid

  selected=$(awk -v wanted="$target" '
    function string_value(line) {
      sub(/^[^:]*:[[:space:]]*"/, "", line)
      sub(/"[[:space:]]*,?[[:space:]]*$/, "", line)
      return line
    }
    /^[[:space:]]*"target":[[:space:]]*"/ {
      active = string_value($0) == wanted
      if (active) matches += 1
      next
    }
    active && /^[[:space:]]*"archive":[[:space:]]*"/ { archive = string_value($0); next }
    active && /^[[:space:]]*"sha256":[[:space:]]*"/ { sha = string_value($0); next }
    END {
      if (matches != 1 || archive == "" || sha == "") exit 1
      print archive "|" sha
    }
  ' "$public_manifest") || fail release-metadata-invalid
  archive_name=${selected%%|*}
  archive_sha=${selected#*|}
  [ "$archive_name" = "muxvia-$release-$target.tar.gz" ] || fail release-metadata-invalid
  printf '%s\n' "$archive_sha" | grep -Eq '^[0-9a-f]{64}$' || fail release-metadata-invalid
}

parse_bundle_files() {
  awk '
    function string_value(line) {
      sub(/^[^:]*:[[:space:]]*"/, "", line)
      sub(/"[[:space:]]*,?[[:space:]]*$/, "", line)
      return line
    }
    function scalar_value(line) {
      sub(/^[^:]*:[[:space:]]*/, "", line)
      sub(/[[:space:]]*,?[[:space:]]*$/, "", line)
      return line
    }
    /^[[:space:]]*"role":[[:space:]]*"/ { role = string_value($0); next }
    role != "" && /^[[:space:]]*"path":[[:space:]]*"/ { path = string_value($0); next }
    role != "" && /^[[:space:]]*"executable":[[:space:]]*/ { executable = scalar_value($0); next }
    role != "" && /^[[:space:]]*"byteLength":[[:space:]]*/ { bytes = scalar_value($0); next }
    role != "" && /^[[:space:]]*"sha256":[[:space:]]*"/ {
      sha = string_value($0)
      print role "|" path "|" executable "|" bytes "|" sha
      role = path = executable = bytes = sha = ""
      count += 1
    }
    END { if (count != 5 || role != "") exit 1 }
  ' "$1"
}

validate_bundle() {
  bundle_root=$1
  manifest=$bundle_root/muxvia-release.json
  [ -d "$bundle_root" ] && [ ! -L "$bundle_root" ] || fail bundle-invalid:root
  [ -f "$manifest" ] && [ ! -L "$manifest" ] || fail bundle-invalid:manifest

  schema=$(sed -n 's/^[[:space:]]*"schemaVersion":[[:space:]]*\([0-9][0-9]*\)[[:space:]]*,\{0,1\}[[:space:]]*$/\1/p' "$manifest")
  product=$(sed -n 's/^[[:space:]]*"product":[[:space:]]*"\([^"]*\)"[[:space:]]*,\{0,1\}[[:space:]]*$/\1/p' "$manifest")
  bundle_release=$(sed -n 's/^[[:space:]]*"release":[[:space:]]*"\([^"]*\)"[[:space:]]*,\{0,1\}[[:space:]]*$/\1/p' "$manifest")
  bundle_target=$(sed -n 's/^[[:space:]]*"target":[[:space:]]*"\([^"]*\)"[[:space:]]*,\{0,1\}[[:space:]]*$/\1/p' "$manifest")
  bundle_build=$(sed -n 's/^[[:space:]]*"build":[[:space:]]*"\([^"]*\)"[[:space:]]*,\{0,1\}[[:space:]]*$/\1/p' "$manifest")
  rpc_major=$(sed -n 's/^[[:space:]]*"major":[[:space:]]*\([0-9][0-9]*\)[[:space:]]*,\{0,1\}[[:space:]]*$/\1/p' "$manifest")
  rpc_minor=$(sed -n 's/^[[:space:]]*"minor":[[:space:]]*\([0-9][0-9]*\)[[:space:]]*,\{0,1\}[[:space:]]*$/\1/p' "$manifest")
  [ "$schema" = 1 ] && [ "$product" = muxvia ] \
    && [ "$bundle_release" = "$release" ] && [ "$bundle_target" = "$target" ] \
    && [ "$rpc_major" = 1 ] && [ "$rpc_minor" = 0 ] \
    || fail bundle-invalid:identity
  printf '%s\n' "$bundle_build" | grep -Eq '^[0-9A-Za-z._-]{7,128}$' || fail bundle-invalid:build

  entries=$(find "$bundle_root" ! -path "$bundle_root" -prune -print | wc -l | tr -d '[:space:]')
  [ "$entries" = 6 ] || fail bundle-invalid:file-set
  parsed=$stage/parsed-bundle-files
  parse_bundle_files "$manifest" > "$parsed" || fail bundle-invalid:manifest-files

  index=0
  while IFS='|' read -r role relative executable byte_length expected_sha; do
    index=$((index + 1))
    case "$index:$role:$relative:$executable" in
      1:control-plane:muxvia:true|2:routing-service:muxvia-routing:true|3:license:LICENSE:false|4:third-party-notices:THIRD_PARTY_NOTICES.md:false|5:extraction-manifest:EXTRACTION_MANIFEST.json:false) ;;
      *) fail bundle-invalid:file-contract ;;
    esac
    printf '%s\n' "$byte_length" | grep -Eq '^(0|[1-9][0-9]*)$' || fail bundle-invalid:file-length
    printf '%s\n' "$expected_sha" | grep -Eq '^[0-9a-f]{64}$' || fail bundle-invalid:file-hash
    member=$bundle_root/$relative
    [ -f "$member" ] && [ ! -L "$member" ] || fail "bundle-invalid:file-type:$role"
    actual_length=$(wc -c < "$member" | tr -d '[:space:]')
    [ "$actual_length" = "$byte_length" ] || fail "bundle-invalid:file-length:$role"
    if [ "$executable" = true ]; then
      [ -x "$member" ] || fail "bundle-invalid:file-mode:$role"
    else
      [ ! -x "$member" ] || fail "bundle-invalid:file-mode:$role"
    fi
    actual_sha=$(sha256_file "$member")
    [ "$actual_sha" = "$expected_sha" ] || fail "bundle-invalid:file-hash:$role"
  done < "$parsed"
  [ "$index" = 5 ] || fail bundle-invalid:file-contract
  validated_build=$bundle_build
}

write_launcher() {
  desired=$stage/muxvia-launcher
  cat > "$desired" <<'EOF'
#!/bin/sh
set -eu
program=muxvia-launcher
fail() { printf '%s:%s\n' "$program" "$1" >&2; exit 1; }
[ -n "${HOME:-}" ] && [ "${HOME#/}" != "$HOME" ] || fail invalid-home
install_root=$HOME/.muxvia/install
owner_file=$install_root/owner
active_file=$install_root/active-version
[ -f "$owner_file" ] && [ ! -L "$owner_file" ] || fail ownership-invalid
[ "$(sed -n '1p' "$owner_file")" = verified-download ] || fail ownership-invalid
[ -f "$active_file" ] && [ ! -L "$active_file" ] || fail active-version-invalid
[ "$(wc -l < "$active_file" | tr -d '[:space:]')" = 1 ] || fail active-version-invalid
IFS= read -r active < "$active_file"
case "$active" in ''|*[!0-9A-Za-z._-]*) fail active-version-invalid ;; esac
bundle=$install_root/versions/$active
[ -d "$bundle" ] && [ ! -L "$bundle" ] || fail active-version-missing
[ -x "$bundle/muxvia" ] && [ ! -L "$bundle/muxvia" ] || fail control-plane-missing
exec "$bundle/muxvia" "$@"
EOF
  chmod 700 "$desired"
  if [ -e "$launcher" ]; then
    [ -f "$launcher" ] && [ ! -L "$launcher" ] || fail ownership-conflict:launcher
    cmp -s "$desired" "$launcher" || fail ownership-conflict:launcher
  else
    mv "$desired" "$launcher" || fail launcher-install-failed
  fi
}

activate() {
  stage_build=$validated_build
  bundle_id=$release-$target-$stage_build
  destination=$versions/$bundle_id
  if [ -e "$destination" ]; then
    [ -d "$destination" ] && [ ! -L "$destination" ] || fail existing-version-invalid
    validate_bundle "$destination"
    [ "$validated_build" = "$stage_build" ] || fail existing-version-invalid
  else
    mv "$bundle_root" "$destination" || fail version-stage-failed
  fi

  write_launcher
  active_temporary=$(mktemp "$install_root/.active-version.XXXXXX") || fail activation-failed
  printf '%s\n' "$bundle_id" > "$active_temporary" || fail activation-failed
  chmod 600 "$active_temporary" || fail activation-failed
  if [ "${MUXVIA_INSTALLER_TESTING:-0}" = 1 ] \
    && [ "${MUXVIA_INSTALLER_TEST_FAIL_BEFORE_ACTIVATION:-0}" = 1 ]; then
    fail activation-failed
  fi
  mv -f "$active_temporary" "$active_file" || fail activation-failed
  printf 'Muxvia %s installed for %s. Add %s to PATH.\n' "$release" "$target" "$bin_dir"
}

[ "$#" -eq 0 ] || fail unexpected-arguments
[ -n "${HOME:-}" ] && [ "${HOME#/}" != "$HOME" ] || fail invalid-home
require_command awk
require_command cmp
require_command curl
require_command find
require_command grep
require_command mktemp
require_command readlink
require_command sed
require_command tar
require_command uname

muxvia_home=$HOME/.muxvia
install_root=$muxvia_home/install
owner_file=$install_root/owner
versions=$install_root/versions
staging=$install_root/staging
active_file=$install_root/active-version
bin_dir=$muxvia_home/bin
launcher=$bin_dir/muxvia

if [ "${MUXVIA_INSTALLER_TESTING:-0}" = 1 ]; then
  manifest_url=${MUXVIA_INSTALLER_TEST_MANIFEST_URL:-$manifest_url}
  releases_url=${MUXVIA_INSTALLER_TEST_RELEASES_URL:-$releases_url}
fi

check_path_ownership
check_install_ownership
detect_target

mkdir -p "$muxvia_home" "$install_root" "$versions" "$staging" "$bin_dir"
chmod 700 "$muxvia_home" "$install_root" "$versions" "$staging" "$bin_dir"
if [ ! -e "$owner_file" ]; then
  owner_temporary=$(mktemp "$install_root/.owner.XXXXXX") || fail owner-install-failed
  printf '%s\n' verified-download > "$owner_temporary"
  mv "$owner_temporary" "$owner_file" || fail owner-install-failed
fi
chmod 600 "$owner_file"

stage=$(mktemp -d "$staging/install.XXXXXX") || fail staging-create-failed
cleanup() {
  rm -rf "$stage"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

download "$manifest_url" "$stage/muxvia-latest.json"
parse_public_manifest "$stage/muxvia-latest.json"
archive=$stage/$archive_name
download "$releases_url/v$release/$archive_name" "$archive"
[ "$(sha256_file "$archive")" = "$archive_sha" ] || fail archive-hash-mismatch

tar -xzf "$archive" -C "$stage" || fail archive-extraction-failed
bundle_root=$stage/muxvia-$release-$target
validate_bundle "$bundle_root"
activate
