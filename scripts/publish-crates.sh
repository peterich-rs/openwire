#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/publish-crates.sh --dry-run [--allow-dirty]
  scripts/publish-crates.sh --publish [--allow-dirty] [--no-wait]

Options:
  --dry-run      Run a first-release-safe package preflight. Cargo checks package
                 file lists for every publishable crate, and builds a tarball for
                 openwire-core because it has no unpublished internal dependency.
  --publish      Publish each crate to crates.io in dependency order.
  --allow-dirty  Pass --allow-dirty through to cargo package/publish.
  --no-wait      Do not wait for each published crate version to become visible
                 before publishing the next dependent crate.
  -h, --help     Show this help text.
USAGE
}

mode=""
allow_dirty=0
wait_for_registry=1

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run)
      mode="dry-run"
      ;;
    --publish)
      mode="publish"
      ;;
    --allow-dirty)
      allow_dirty=1
      ;;
    --no-wait)
      wait_for_registry=0
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

if [[ -z "$mode" ]]; then
  echo "missing mode: pass --dry-run or --publish" >&2
  usage >&2
  exit 2
fi

readonly publish_order=(
  openwire-core
  openwire-tokio
  openwire-rustls
  openwire
  openwire-cache
  openwire-fastwebsockets
  openwire-tungstenite
)

cargo_args=()
if [[ "$allow_dirty" -eq 1 ]]; then
  cargo_args+=(--allow-dirty)
fi

crate_version() {
  cargo pkgid -p "$1" | sed 's/.*#//'
}

workspace_version="$(crate_version openwire-core)"
for crate in "${publish_order[@]}"; do
  version="$(crate_version "$crate")"
  if [[ "$version" != "$workspace_version" ]]; then
    echo "version mismatch: openwire-core is $workspace_version but $crate is $version" >&2
    exit 1
  fi
done

assert_internal_dependency_versions_match() {
  local dependency
  local expected="version = \"$workspace_version\""

  for dependency in \
    openwire \
    openwire-cache \
    openwire-core \
    openwire-fastwebsockets \
    openwire-rustls \
    openwire-tokio \
    openwire-tungstenite
  do
    if ! grep -Fq "$dependency = {" Cargo.toml; then
      echo "missing workspace dependency entry for $dependency" >&2
      exit 1
    fi
    if ! grep -F "$dependency = {" Cargo.toml | grep -Fq "$expected"; then
      echo "workspace dependency $dependency must use $expected" >&2
      exit 1
    fi
  done
}

wait_until_visible() {
  local crate="$1"
  local version="$2"
  local attempt

  for attempt in {1..30}; do
    if cargo info "${crate}@${version}" >/dev/null 2>&1; then
      return 0
    fi
    echo "waiting for ${crate} ${version} to become visible on crates.io (${attempt}/30)"
    sleep 10
  done

  echo "${crate} ${version} was not visible on crates.io after waiting" >&2
  return 1
}

assert_internal_dependency_versions_match

echo "OpenWire release version: ${workspace_version}"

for crate in "${publish_order[@]}"; do
  echo "==> ${mode}: ${crate}"
  if [[ "$mode" == "dry-run" ]]; then
    cargo package -p "$crate" --list "${cargo_args[@]}" >/dev/null
    if [[ "$crate" == "openwire-core" ]]; then
      cargo package -p "$crate" --no-verify "${cargo_args[@]}"
    else
      echo "checked package file list; tarball packaging waits for staged registry dependencies"
    fi
  else
    cargo publish -p "$crate" "${cargo_args[@]}"
    if [[ "$wait_for_registry" -eq 1 ]]; then
      wait_until_visible "$crate" "$workspace_version"
    fi
  fi
done
