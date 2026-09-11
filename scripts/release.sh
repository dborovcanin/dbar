#!/bin/sh

# Tags the version in Cargo.toml and pushes it. That push is the release: the
# workflow builds dbar-linux-x86_64, checksums it, and creates the GitHub
# release. Nothing here builds or uploads anything.

set -eu

die() {
    printf 'release: %s\n' "$*" >&2
    exit 1
}

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

command -v git >/dev/null 2>&1 || die "git is required"

version=$(sed -n 's/^version *= *"\([^"]*\)"/\1/p' Cargo.toml | head -n 1)
[ -n "$version" ] || die "could not read the package version from Cargo.toml"
tag="v$version"

# The tag is what users download from, so it has to name a commit that is
# already public and reviewed, not a local state nobody else can see.
[ -z "$(git status --porcelain)" ] || die "the worktree must be clean"
[ "$(git branch --show-current)" = main ] || die "releases must be made from main"

git fetch --tags origin
head_commit=$(git rev-parse HEAD)
[ "$head_commit" = "$(git rev-parse refs/remotes/origin/main)" ] ||
    die "HEAD must match origin/main"

if tagged_commit=$(git rev-parse -q --verify "refs/tags/$tag^{commit}"); then
    [ "$tagged_commit" = "$head_commit" ] ||
        die "$tag already points at $tagged_commit; bump the version in Cargo.toml"
else
    git tag -a "$tag" -m "Release $tag"
fi

git push origin "refs/tags/$tag"

printf 'Pushed %s. The Release workflow builds and publishes the assets.\n' "$tag"
