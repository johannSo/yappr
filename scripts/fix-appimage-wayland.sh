#!/usr/bin/env bash
# Repack an AppImage so it runs as a native Wayland client with the host's
# libwayland — instead of being forced onto XWayland with a bundled one.
#
# Why this exists (verified on Arch/Hyprland, mesa 26, 2026-09-01):
#
# 1. linuxdeploy-plugin-gtk's AppRun hook hardcodes `export GDK_BACKEND=x11`.
#    Under X11 there is no wlr-layer-shell, so layer.rs falls back to a plain
#    toplevel and the overlay loses its protocol-level focus refusal
#    (invariant 2's strong half) and its bottom-anchor placement.
# 2. linuxdeploy bundles the build distro's libwayland-*.so. Those load first
#    (rpath), and the host mesa's EGL vendor library then fails to resolve
#    against the older libwayland-client — leaving glvnd with no vendors, so
#    eglGetPlatformDisplay() answers EGL_BAD_PARAMETER for *every* platform
#    and WebKit aborts: "Could not create default EGL display". This kills
#    the webviews on any host whose mesa is newer than the bundled wayland,
#    on the X11 backend as well.
#
# The fix is the AppImage excludelist's own position on libwayland: don't
# ship it. Every Wayland-capable host has a current one, and X11-only hosts
# never load it. The hook edit keeps a user override working
# (GDK_BACKEND=x11 ./yappr.AppImage) while defaulting to wayland,x11.
#
# Usage: scripts/fix-appimage-wayland.sh path/to/yappr_x.y.z_amd64.AppImage
# The file is replaced in place. Needs: bash, wget or curl (only if
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

hook=squashfs-root/apprun-hooks/linuxdeploy-plugin-gtk.sh
if [ ! -f "$hook" ]; then
    echo "error: $hook not in $appimage — linuxdeploy layout changed?" >&2
    exit 1
fi
grep -q '^export GDK_BACKEND=x11' "$hook" || {
    echo "error: no 'export GDK_BACKEND=x11' line in $hook — already fixed, or the plugin changed" >&2
    exit 1
}
sed -i 's/^export GDK_BACKEND=x11.*/export GDK_BACKEND="${GDK_BACKEND:-wayland,x11}"/' "$hook"

removed=$(ls squashfs-root/usr/lib/libwayland-*.so* 2>/dev/null || true)
if [ -z "$removed" ]; then
    echo "error: no bundled libwayland in $appimage — nothing to fix, or the layout changed" >&2
    exit 1
fi
rm -f squashfs-root/usr/lib/libwayland-*.so*

# Absolute paths, not relative. appimagetool is itself an AppImage and runs
# here under APPIMAGE_EXTRACT_AND_RUN, which extracts it and executes from a
# different working directory -- so a relative "squashfs-root" resolves
# against the wrong place and it fails with the unhelpful
# "Error: no such file or directory: squashfs-root" while the directory is
# sitting right there in $workdir.
ARCH=x86_64 "$APPIMAGETOOL" --runtime-file "$workdir/runtime" \
    "$workdir/squashfs-root" "$workdir/repacked.AppImage" >&2
mv "$workdir/repacked.AppImage" "$appimage"
echo "$appimage now defaults to the Wayland backend; removed:" $(basename -a $removed)
