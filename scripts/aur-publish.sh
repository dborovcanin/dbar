#!/bin/bash

# Brings the AUR packages up to a released version.
#
#   scripts/aur-publish.sh 0.5.0             update the PKGBUILDs and push both
#   scripts/aur-publish.sh --no-push 0.5.0   update them and stop, for review
#   scripts/aur-publish.sh --pkgrel 2 0.5.0  republish a version whose packaging
#                                            changed but whose source did not
#   scripts/aur-publish.sh --only dbar-bin 0.5.0   leave the other package alone
#
# The checksums come from the files themselves rather than from anything typed
# here, so the release has to exist before this runs: `dbar` needs the tag
# archive and `dbar-bin` needs the binary the Release workflow uploads. Running
# it before that fails at the download rather than publishing a wrong sum.
#
# The PKGBUILDs in the repository are rewritten in place, so what is committed
# here is what the AUR was given. Run it with --no-push before tagging to keep
# them in step, or let the Release workflow run it and commit the result after.

set -euo pipefail

die() {
    printf 'aur-publish: %s\n' "$*" >&2
    exit 1
}

# Checks every release asset against the checksum the release itself publishes.
#
# updpkgsums learns a sum by downloading, so it records whatever was being
# served at that moment. A release built twice - two workflow runs for one tag,
# an asset replaced by hand - serves two different binaries, and the sum can
# end up naming one that is no longer there. The release publishes a .sha256
# beside each asset; disagreeing with it means the download raced something,
# and publishing that would hand every user a package that cannot install.
verify_release_assets() {
    srcinfo=$1
    sources=$(sed -n 's/^\tsource = //p' "$srcinfo")
    sums=$(sed -n 's/^\tsha256sums = //p' "$srcinfo")

    line=0
    while [ "$line" -lt "$(printf '%s\n' "$sources" | wc -l)" ]; do
        line=$((line + 1))
        source=$(printf '%s\n' "$sources" | sed -n "${line}p")
        sum=$(printf '%s\n' "$sums" | sed -n "${line}p")

        url=${source#*::}
        case $url in
        *"/releases/download/"*) ;;
        *) continue ;;
        esac

        published=$(curl -fsSL "$url.sha256" 2>/dev/null | awk '{print $1}') || published=
        if [ -z "$published" ]; then
            printf 'no %s.sha256 to check against; trusting the download\n' \
                "${url##*/}" >&2
            continue
        fi

        [ "$sum" = "$published" ] || die "$(
            printf '%s\n' \
                "the checksum for ${url##*/} does not match the one the release publishes." \
                "  downloaded now: $sum" \
                "  release says:   $published" \
                "Something replaced the asset after it was published - most often a" \
                "second workflow run for the same tag. Nothing was pushed."
        )"
        printf 'checked %s against its published sha256\n' "${url##*/}"
    done
}

push=yes
pkgrel=1
only=
while :; do
    case ${1-} in
    --no-push)
        push=no
        shift
        ;;
    # A released version whose packaging has to change - a checksum corrected,
    # a dependency fixed - keeps its pkgver and counts up here instead, which
    # is what tells everyone's helper that there is something new to take.
    --pkgrel)
        pkgrel=${2-}
        case $pkgrel in
        '' | *[!0-9]*) die "--pkgrel wants a number (got '${2-}')" ;;
        esac
        shift 2
        ;;
    # Repairing one package should not touch the other: a corrected checksum in
    # dbar-bin is no reason for everyone building dbar from source to rebuild it.
    --only)
        only=${2-}
        case $only in
        dbar | dbar-bin) ;;
        *) die "--only wants dbar or dbar-bin (got '${2-}')" ;;
        esac
        shift 2
        ;;
    *) break ;;
    esac
done

version=${1-}
[ -n "$version" ] || die "usage: $0 [--no-push] [--pkgrel N] [--only PKG] <version>"
case $version in
v*) die "give the version without the leading v (got $version)" ;;
esac

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

for tool in makepkg updpkgsums git; do
    command -v "$tool" >/dev/null 2>&1 ||
        die "$tool is required (pacman -S base-devel pacman-contrib git)"
done

# makepkg refuses to run as root, and it is the thing that reads the PKGBUILD.
[ "$(id -u)" -ne 0 ] || die "run as an ordinary user, not root"

declared=$(sed -n 's/^version *= *"\([^"]*\)"/\1/p' Cargo.toml | head -n 1)
[ "$declared" = "$version" ] ||
    die "Cargo.toml says $declared, not $version; bump it or pass the right version"

known_hosts=$root/packaging/aur/known_hosts
[ -f "$known_hosts" ] || die "missing $known_hosts"

# The AUR's host keys come from the repository rather than from whatever
# answers on the day, and StrictHostKeyChecking makes that pinning mean
# something. AUR_SSH_KEY names the key to push with when the caller has one
# outside the usual place, which is what the release workflow sets.
ssh_command="ssh -o UserKnownHostsFile=$known_hosts -o StrictHostKeyChecking=yes"
if [ -n "${AUR_SSH_KEY-}" ]; then
    [ -r "$AUR_SSH_KEY" ] || die "AUR_SSH_KEY names $AUR_SSH_KEY, which cannot be read"
    ssh_command="$ssh_command -i $AUR_SSH_KEY -o IdentitiesOnly=yes"
fi
export GIT_SSH_COMMAND=$ssh_command

for pkgname in dbar dbar-bin; do
    [ -z "$only" ] || [ "$only" = "$pkgname" ] || continue
    dir=$root/packaging/aur/$pkgname
    [ -f "$dir/PKGBUILD" ] || die "missing $dir/PKGBUILD"

    printf '\n== %s %s ==\n' "$pkgname" "$version"

    sed -i \
        -e "s/^pkgver=.*/pkgver=$version/" \
        -e "s/^pkgrel=.*/pkgrel=$pkgrel/" \
        "$dir/PKGBUILD"

    # updpkgsums downloads every source and writes the real sums back, which is
    # the one step that must not be done by hand. It leaves what it downloaded
    # in the package directory, and none of that belongs in the repository.
    (cd "$dir" && updpkgsums)
    (cd "$dir" && makepkg --printsrcinfo >.SRCINFO)
    find "$dir" -mindepth 1 -not -name PKGBUILD -not -name .SRCINFO -delete

    grep -q "^	pkgver = $version$" "$dir/.SRCINFO" ||
        die "$pkgname/.SRCINFO does not say $version after the rewrite"
    if grep -q 'sha256sums = SKIP' "$dir/.SRCINFO"; then
        die "$pkgname still has a SKIP checksum; updpkgsums did not run"
    fi

    verify_release_assets "$dir/.SRCINFO"

    [ "$push" = yes ] || continue

    checkout=$(mktemp -d)
    trap 'rm -rf "$checkout"' EXIT
    git clone --quiet "ssh://aur@aur.archlinux.org/$pkgname.git" "$checkout"

    cp "$dir/PKGBUILD" "$dir/.SRCINFO" "$checkout/"
    # --porcelain rather than `diff`, because the first push to a package that
    # does not exist yet has no tracked files for a diff to find.
    if [ -z "$(git -C "$checkout" status --porcelain)" ]; then
        printf '%s is already at %s on the AUR\n' "$pkgname" "$version"
        rm -rf "$checkout"
        trap - EXIT
        continue
    fi

    git -C "$checkout" add PKGBUILD .SRCINFO
    git -C "$checkout" commit --quiet -m "Update to $version-$pkgrel"
    # HEAD:master rather than master, because a clone of a package that does not
    # exist yet starts on whatever init.defaultBranch says, and the AUR wants
    # master either way.
    git -C "$checkout" push --quiet origin HEAD:master
    printf 'Pushed %s %s to the AUR\n' "$pkgname" "$version"

    rm -rf "$checkout"
    trap - EXIT
done

printf '\nDone. The PKGBUILDs in packaging/aur are what the AUR now has.\n'
