#!/bin/sh
# Builds the release binaries and packages them into dist/:
#
#   fast-search-x86_64.tar.gz     the three binaries
#   fast-search-amd64.deb         Debian, Ubuntu, Mint, ...
#   fast-search-x86_64.rpm        Fedora, openSUSE, RHEL, ...   (needs rpmbuild)
#   fast-search-x86_64.AppImage   any distribution              (needs mksquashfs, curl)
#
# File names carry no version so that a "latest release" download link
# stays the same from one release to the next. A package whose build tool
# is not installed is skipped with a note.
#
#   packaging/build-packages.sh           build everything possible
#   SKIP_BUILD=1 packaging/build-packages.sh   reuse the existing binaries
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$here/.."

# A release build takes its version from the tag ($FF_VERSION), which
# the programs report too; otherwise Cargo.toml's.
version=${FF_VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)}
rel=target/x86_64-unknown-linux-musl/release
if [ -z "${SKIP_BUILD:-}" ]; then
    # `cargo dist` also rebuilds the standard library for size; that needs
    # a C linker for its build scripts. Without one, fall back to the
    # ordinary release build (binaries are a little larger).
    if command -v cc >/dev/null 2>&1; then
        cargo dist
    else
        echo "build-packages: no C linker (cc): making an ordinary release build"
        cargo build --release
    fi
fi
# Guard the size: every binary must stay under 2 MB.
for b in ff ff-gui ffd; do
    size=$(wc -c < "target/x86_64-unknown-linux-musl/release/$b" 2>/dev/null || echo 0)
    if [ "$size" -gt 2097152 ]; then
        echo "build-packages: $b is $size bytes, over the 2 MB limit" >&2
        exit 1
    fi
done
for b in ff ff-gui ffd; do
    [ -x "$rel/$b" ] || { echo "build-packages: $rel/$b is missing" >&2; exit 1; }
done

dist=$PWD/dist
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
rm -rf "$dist"
mkdir -p "$dist"
have() { command -v "$1" >/dev/null 2>&1; }
summary="Fast search finds any file on the disk by name in milliseconds."

# ---- the files every package installs ---------------------------------------
root=$stage/root
for b in ff ff-gui ffd; do
    install -D -m 755 "$rel/$b" "$root/usr/bin/$b"
done
install -D -m 644 docs/icon.svg "$root/usr/share/icons/hicolor/scalable/apps/ff-gui.svg"
install -D -m 644 README.md "$root/usr/share/doc/fast-search/README.md"
install -D -m 644 LICENSE "$root/usr/share/doc/fast-search/LICENSE"
# The file name must stay ff-gui.desktop: desktops match it to the window.
install -D -m 644 packaging/ff-gui.desktop "$root/usr/share/applications/ff-gui.desktop"

# ---- tar.gz -------------------------------------------------------------------
mkdir "$stage/fast-search-$version"
cp "$rel/ff" "$rel/ff-gui" "$rel/ffd" README.md LICENSE "$stage/fast-search-$version/"
tar -C "$stage" --owner=0 --group=0 -czf "$dist/fast-search-x86_64.tar.gz" "fast-search-$version"
echo "built   fast-search-x86_64.tar.gz"

# ---- deb ----------------------------------------------------------------------
# A .deb is an `ar` archive of three members; both formats are simple
# enough to write here, so no Debian tools are needed.
deb=$stage/deb
mkdir "$deb"
cat > "$deb/control" <<EOF
Package: fast-search
Version: $version
Architecture: amd64
Maintainer: Zied <khunder@users.noreply.github.com>
Installed-Size: $(du -sk "$root" | cut -f1)
Section: utils
Priority: optional
Homepage: https://github.com/khunder/fast-search
Description: Instant file search for Linux
 $summary
 It provides the ff command, the Fast search window (ff-gui) and the
 optional ffd daemon that keeps the index current.
EOF
tar -C "$deb" --owner=0 --group=0 -czf "$deb/control.tar.gz" ./control
tar -C "$root" --owner=0 --group=0 -czf "$deb/data.tar.gz" ./usr
printf '2.0\n' > "$deb/debian-binary"
ar_member() { # file -> one `ar` archive member on stdout
    size=$(wc -c < "$1" | tr -d ' ')
    printf '%-16s%-12s%-6s%-6s%-8s%-10s`\n' "$(basename "$1")" "$(date +%s)" 0 0 100644 "$size"
    cat "$1"
    [ $((size % 2)) -eq 0 ] || printf '\n'   # members are padded to even length
}
{
    printf '!<arch>\n'
    ar_member "$deb/debian-binary"
    ar_member "$deb/control.tar.gz"
    ar_member "$deb/data.tar.gz"
} > "$dist/fast-search-amd64.deb"
echo "built   fast-search-amd64.deb"

# ---- rpm ----------------------------------------------------------------------
if have rpmbuild; then
    rpm=$stage/rpm
    mkdir -p "$rpm/SPECS"
    cat > "$rpm/SPECS/fast-search.spec" <<EOF
Name:           fast-search
Version:        $version
Release:        1
Summary:        Instant file search for Linux
License:        MIT
URL:            https://github.com/khunder/fast-search
BuildArch:      x86_64
AutoReqProv:    no

# The binaries are already built, static and stripped: just package them.
%global debug_package %{nil}
%global __strip /bin/true
%global __os_install_post %{nil}
%global _build_id_links none

%description
$summary
It provides the ff command, the Fast search window (ff-gui) and the
optional ffd daemon that keeps the index current.

%install
mkdir -p %{buildroot}
cp -a "$root/." %{buildroot}/

%files
/usr/bin/ff
/usr/bin/ff-gui
/usr/bin/ffd
/usr/share/applications/ff-gui.desktop
/usr/share/icons/hicolor/scalable/apps/ff-gui.svg
/usr/share/doc/fast-search
EOF
    rpmbuild -bb --quiet --target x86_64 --define "_topdir $rpm" "$rpm/SPECS/fast-search.spec"
    cp "$rpm/RPMS/x86_64/fast-search-$version-1.x86_64.rpm" "$dist/fast-search-x86_64.rpm"
    echo "built   fast-search-x86_64.rpm"
else
    echo "skipped fast-search-x86_64.rpm (rpmbuild is not installed)"
fi

# ---- AppImage -------------------------------------------------------------------
# An AppImage is a small launcher program followed by a compressed image
# of the application folder.
if have mksquashfs && have curl; then
    app=$stage/AppDir
    mkdir "$app"
    cp -a "$root/usr" "$app/usr"
    cp "$root/usr/share/applications/ff-gui.desktop" "$app/ff-gui.desktop"
    cp docs/icon.svg "$app/ff-gui.svg"
    ln -s ff-gui.svg "$app/.DirIcon"
    # Starts the window by default; "ff ..." or "ffd ..." as first
    # arguments reach the other two programs.
    cat > "$app/AppRun" <<'EOF'
#!/bin/sh
here=$(dirname "$(readlink -f "$0")")
case "${1:-}" in
    ff|ffd|ff-gui) tool=$1; shift; exec "$here/usr/bin/$tool" "$@" ;;
esac
exec "$here/usr/bin/ff-gui" "$@"
EOF
    chmod 755 "$app/AppRun"
    curl -fsSL -o "$stage/runtime" \
        https://github.com/AppImage/type2-runtime/releases/download/continuous/runtime-x86_64
    mksquashfs "$app" "$stage/app.squashfs" -root-owned -noappend -comp zstd -quiet -no-progress
    cat "$stage/runtime" "$stage/app.squashfs" > "$dist/fast-search-x86_64.AppImage"
    chmod 755 "$dist/fast-search-x86_64.AppImage"
    echo "built   fast-search-x86_64.AppImage"
else
    echo "skipped fast-search-x86_64.AppImage (needs mksquashfs and curl)"
fi

echo
ls -l "$dist"
