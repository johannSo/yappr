#!/usr/bin/env bash
# Repack an AppImage so its root icon and .DirIcon are the 256x256 icon.
#
# Why this exists: `tauri build` copies the largest icon to the AppDir root,
# but the linuxdeploy build it pins (1-alpha, 659c9db) then overwrites that
# with a symlink to whichever hicolor icon it happens to list first — the
# 32x32 one, in practice. `.DirIcon` is what desktop integrators (Gear Lever,
# appimaged, AppImageLauncher) extract for the app menu, so every new user
# got a 32px menu icon scaled up. The AppImage spec recommends a 256x256
# `.DirIcon`; this script makes that true after the fact, since Tauri offers
# no hook between linuxdeploy's root setup and the squashfs packing.
#
# Usage: scripts/fix-appimage-icon.sh path/to/yappr_x.y.z_amd64.AppImage
# The file is replaced in place. Needs: bash, file, wget or curl (only if
# appimagetool is not already on PATH or in $APPIMAGETOOL).
set -euo pipefail

appimage=$(realpath "${1:?usage: $0 <AppImage>}")

# appimagetool: honor $APPIMAGETOOL, then PATH, else download the continuous
# build once into a cache dir. It runs fine without FUSE via extract-and-run.
export APPIMAGE_EXTRACT_AND_RUN=1
if [ -z "${APPIMAGETOOL:-}" ]; then
    if command -v appimagetool >/dev/null; then
        APPIMAGETOOL=$(command -v appimagetool)
    else
        cache="${XDG_CACHE_HOME:-$HOME/.cache}/yappr-build"
        APPIMAGETOOL="$cache/appimagetool-x86_64.AppImage"
        if [ ! -x "$APPIMAGETOOL" ]; then
            mkdir -p "$cache"
            url="https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage"
            if command -v wget >/dev/null; then wget -q -O "$APPIMAGETOOL" "$url"; else curl -fsSL -o "$APPIMAGETOOL" "$url"; fi
            chmod +x "$APPIMAGETOOL"
        fi
    fi
fi

workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT
cd "$workdir"

# The first --appimage-offset bytes ARE the runtime; reusing them keeps the
# repacked file byte-compatible with what linuxdeploy shipped and saves
# appimagetool a network fetch for one.
offset=$("$appimage" --appimage-offset)
head -c "$offset" "$appimage" > runtime

"$appimage" --appimage-extract >/dev/null

name=$(basename "$(readlink -f squashfs-root/.DirIcon 2>/dev/null || echo squashfs-root/*.png)")
name=${name%.png}
src=""
for p in squashfs-root/usr/share/icons/hicolor/*/apps/"$name".png; do
    if file "$p" | grep -q '256 x 256'; then src="$p"; break; fi
done
if [ -z "$src" ]; then
    echo "error: no 256x256 $name.png inside $appimage" >&2
    exit 1
fi

rm -f "squashfs-root/$name.png" squashfs-root/.DirIcon
cp "$src" "squashfs-root/$name.png"
ln -s "$name.png" squashfs-root/.DirIcon

ARCH=x86_64 "$APPIMAGETOOL" --runtime-file runtime squashfs-root repacked.AppImage >&2
mv repacked.AppImage "$appimage"
echo "root icon of $appimage is now $(file "squashfs-root/$name.png" | grep -o '[0-9]* x [0-9]*')"
