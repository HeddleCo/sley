#!/usr/bin/env bash
# Run packages separately so workspace feature unification cannot hide leaks.
set -euo pipefail
cd "$(dirname "$0")/.."

for package in sley-transport sley-remote sley; do
    feature_args=()
    case "$package" in
        sley-remote) feature_args=(--features http) ;;
        sley) feature_args=(--features remote) ;;
    esac
    tree=$(cargo tree --locked -p "$package" --no-default-features "${feature_args[@]}" -e normal --prefix none)
    if grep -E '^(ureq|rustls|native-tls|sley-worktree|sley-hooks|sley-sequencer|sley-unpack-trees) v' <<< "$tree"; then
        echo "Unexpected default HTTP or local-operation dependency in $package" >&2
        exit 1
    fi
    cargo clippy --locked -p "$package" --no-default-features "${feature_args[@]}" --all-targets --no-deps -- -D warnings
done

cargo clippy --locked -p sley-remote --no-default-features --all-targets --no-deps -- -D warnings
cargo check --locked -p sley --no-default-features --features remote,worktree
cargo check --locked -p sley --no-default-features --features default-http-client
for tls_backend in tls-rustls tls-native-tls tls-platform-verifier; do
    cargo check --locked -p sley --no-default-features --features "$tls_backend"
done
cargo test --locked -p sley-remote --no-default-features --features http
