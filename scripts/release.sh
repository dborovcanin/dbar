#!/bin/sh

set -eu

die() {
    printf 'release: %s\n' "$*" >&2
    exit 1
}

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

bundle_only=false
case "${1-}" in
    --bundle-only) bundle_only=true ;;
    "") ;;
    *) die "usage: $0 [--bundle-only]" ;;
esac

[ "$(uname -s)" = Linux ] || die "release binaries must be built on Linux"
[ "$(uname -m)" = x86_64 ] || die "release binaries must be built on x86-64"

version=$(sed -n 's/^version *= *"\([^"]*\)"/\1/p' Cargo.toml | head -n 1)
[ -n "$version" ] || die "could not read the package version from Cargo.toml"
tag="v$version"
asset_dir=target/release-assets
asset="$asset_dir/dbar-linux-x86_64"
checksum="$asset.sha256"
cargo=${CARGO:-cargo}

if [ "$bundle_only" = false ]; then
    command -v git >/dev/null 2>&1 || die "git is required"
    command -v gh >/dev/null 2>&1 || die "GitHub CLI (gh) is required"
    command -v sha256sum >/dev/null 2>&1 || die "sha256sum is required"

    [ -z "$(git status --porcelain)" ] || die "the worktree must be clean"
    [ "$(git branch --show-current)" = main ] || die "releases must be made from main"
    gh auth status >/dev/null

    git fetch --tags origin
    head_commit=$(git rev-parse HEAD)
    remote_main=$(git rev-parse refs/remotes/origin/main)
    [ "$head_commit" = "$remote_main" ] || die "HEAD must match origin/main"

    if tagged_commit=$(git rev-parse -q --verify "refs/tags/$tag^{commit}"); then
        if [ "$tagged_commit" != "$head_commit" ]; then
            printf 'Resuming %s from existing commit %s\n' "$tag" "$tagged_commit"
        fi
    fi
fi

"$cargo" build --release --locked
mkdir -p "$asset_dir"
install -m 755 target/release/dbar "$asset"
(
    cd "$asset_dir"
    sha256sum "$(basename "$asset")" >"$(basename "$checksum")"
)

printf 'Bundled %s\n' "$asset"
printf 'Checksum %s\n' "$checksum"

[ "$bundle_only" = true ] && exit 0

if ! git rev-parse -q --verify "refs/tags/$tag^{commit}" >/dev/null; then
    git tag -a "$tag" -m "Release $tag"
fi
git push origin "refs/tags/$tag"

workflow=release.yml
previous_run=$(gh run list \
    --workflow "$workflow" \
    --event workflow_dispatch \
    --commit "$head_commit" \
    --limit 1 \
    --json databaseId \
    --jq '.[0].databaseId // empty')
run_url=$(gh workflow run "$workflow" --ref main --raw-field "tag=$tag")

case "$run_url" in
    */actions/runs/*) run_id=${run_url##*/} ;;
    *) run_id= ;;
esac

attempt=0
while [ -z "$run_id" ] && [ "$attempt" -lt 30 ]; do
    run_id=$(gh run list \
        --workflow "$workflow" \
        --event workflow_dispatch \
        --commit "$head_commit" \
        --limit 1 \
        --json databaseId \
        --jq '.[0].databaseId // empty')
    if [ -n "$run_id" ] && [ "$run_id" != "$previous_run" ]; then
        break
    fi
    run_id=
    attempt=$((attempt + 1))
    sleep 2
done

[ -n "$run_id" ] || die "could not find the dispatched release workflow run"
gh run watch "$run_id" --exit-status

gh release view "$tag" --json url --jq .url
