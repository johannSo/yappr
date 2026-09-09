#!/usr/bin/env bash
# Regenerates the WOFF2 files in `public/fonts/` from pinned upstream sources.
#
# The output of this script is checked in, so you only need to run it to bump a
# font version or to widen the character set. It needs network access and
# `uv` (for fonttools); nothing else, and nothing at app build time.
#
# Why three families and why these three: see the `--font-*` token comments at
# the top of `src/Settings.css`. Why they are bundled at all rather than named
# and hoped for: see the same place, and invariant "the app ships its own type"
# in CLAUDE.md.
#
# ---------------------------------------------------------------- licensing
#
# All three families are SIL OFL 1.1, but they are NOT handled the same way,
# and the difference is not cosmetic:
#
#   Adwaita Sans / Adwaita Mono declare no Reserved Font Name, so we may subset
#   them and still call them by their names.
#
#   iA Writer Duo declares the RFNs "iA Writer" (and "Plex", from the IBM Plex
#   design it is based on). OFL-FAQ 2.6 is explicit that subsetting a webfont
#   -- "removing any parts of the font ..., including unused glyphs" -- is a
#   modification, and a Modified Version may not carry an RFN. OFL-FAQ 2.7/2.8
#   carve out one exception: an optimisation that preserves *Functional
#   Equivalence* may keep the RFN, and its first requirement is "supports the
#   same full character inventory". A glyph subset fails that by construction;
#   a container change from TTF to WOFF2 meets it, along with the other three
#   (same shaping tables, no visual degradation, metadata preserved).
#
#   So iA Writer Duo is converted and NOT subset. It is 1182 glyphs and lands
#   around 70 KB, which is a small price for not having to rename someone
#   else's typeface. If you ever add `--unicodes` to that one, you must also
#   rewrite its internal name records to a name of our own (OFL-FAQ 3.1) --
#   at which point you have forked a typeface, so don't.
#
# `--name-IDs='*'` on every call is load-bearing for the same reason: it keeps
# name IDs 0, 13 and 14 (copyright, licence, licence URL) inside the WOFF2, so
# each file still says what it is and who made it. `public/fonts/OFL-*.txt`
# carries the full licence text alongside, per OFL 1.1 clause 2.

set -euo pipefail

cd "$(dirname "$0")/.."
OUT="public/fonts"
SRC="$(mktemp -d)"
trap 'rm -rf "$SRC"' EXIT

# -- pins ---------------------------------------------------------------------
# Bump these deliberately: a font version changes metrics, and metrics change
# where every label in the settings window wraps.
ADWAITA_TAG="51.0"
IA_COMMIT="f32c04c3058a75d7ce28919ce70fe8800817491b"

ADWAITA_RAW="https://gitlab.gnome.org/GNOME/adwaita-fonts/-/raw/${ADWAITA_TAG}"
IA_RAW="https://raw.githubusercontent.com/iaolo/iA-Fonts/${IA_COMMIT}"

# -- character set ------------------------------------------------------------
# Wide enough that no *label* in this app and no plausible *dictation* in a
# Western or Central European language falls back, narrow enough that the UI
# face stays close to 100 KB. Anything outside this still renders -- CSS font
# fallback is per-glyph, and the stacks in `src/Settings.css` still name system
# families behind ours -- it just renders in a different face.
#
# The ranges are picked by what they cost, measured on Adwaita Sans Regular:
# Latin-1 alone is already 71 KB (a two-axis variable font pays for its delta
# tables before it draws a single glyph), Latin Extended-A adds 8 KB and buys
# Polish, Czech, Hungarian and Turkish, and everything below is single digits.
#
# Deliberately excluded, having been measured: all of Latin Extended-B
# (U+0180-024F, +34 KB, almost entirely Africanist and phonetic letters -- the
# handful of European ones are listed back in by hand), the full combining
# block (U+0300-036F, +23 KB, and only reachable by decomposed text, which
# neither the ASR output nor a typed config value is), the full spacing
# modifiers block (+7 KB), and Greek, Cyrillic and Latin Extended Additional
# (Vietnamese), which together roughly triple the file for scripts this app
# ships no ASR model for. U+1E9E is pulled back in by hand: it is capital
# eszett, which is German and therefore not optional.
UNICODES=$(tr -d ' \n' <<'EOF'
  U+0000-00FF,
  U+0100-017F,
  U+0192,U+01FA-01FF,U+0218-021B,U+0259,
  U+02BB-02BC,U+02C6,U+02DA,U+02DC,
  U+0300-0304,U+0307-0308,U+030A-030C,U+0327-0328,
  U+1E9E,
  U+2000-206F,
  U+2070-209F,
  U+20A0-20BF,
  U+2100-214F,
  U+2150-218F,
  U+2190-2199,U+21A9,
  U+2212,U+2215,U+2219,U+2248,U+2260,U+2264-2265,
  U+25A0,U+25B2-25BC,U+25CF,
  U+2713,U+2717,
  U+FB00-FB06,
  U+FEFF,U+FFFD
EOF
)

# Keep everything that changes how text is *shaped*, not just what it looks
# like. `tnum` is the one that would be missed: `font-variant-numeric:
# tabular-nums` on the overlay timer and the settings counters is a no-op
# without it, and the capsule twitches a pixel wider every tenth of a second.
FEATURES='kern,liga,clig,calt,ccmp,locl,mark,mkmk,rlig,tnum,frac,numr,dnom,zero,case,ordn,sups,subs'

subset() { uv run --quiet --with 'fonttools[woff]' --with brotli pyftsubset "$@"; }
instance() { uv run --quiet --with 'fonttools[woff]' --with brotli \
  python -m fontTools.varLib.instancer "$@"; }

mkdir -p "$OUT"
echo "sources -> $SRC"

# -- Adwaita Sans, upright (UI) -----------------------------------------------
# Kept variable, both axes: opsz 14-32 and wght 100-900. The whole reason to
# take the variable build is that `font-weight: 450` and `650` in the CSS are
# then real instances rather than a browser faking an in-between, and that
# `font-optical-sizing: auto` -- which every engine applies by default -- has
# an axis to act on across the 10.4px..25.6px this face is set at.
curl -sSfL -o "$SRC/AdwaitaSans-Regular.ttf" "$ADWAITA_RAW/sans/AdwaitaSans-Regular.ttf"
subset "$SRC/AdwaitaSans-Regular.ttf" \
  --output-file="$OUT/AdwaitaSans-Regular.subset.woff2" \
  --flavor=woff2 --unicodes="$UNICODES" \
  --layout-features="$FEATURES" --name-IDs='*'

# -- Adwaita Sans, italic (UI) ------------------------------------------------
# Pinned to a single instance and shipped static, which takes it from 195 KB to
# 39 KB. It can afford that because of how little it does: the only italic in
# either window is one `<em>` in the wizard's ydotool note, in body copy, at
# weight 400. Keeping a two-axis variable italic resident for that is five
# times the bytes of the upright's entire weight range.
#
# The cost is real but bounded: an `<em>` inside a heading would get a
# synthesised bold rather than a drawn one. If that ever becomes a thing this
# UI does, drop the `instance` call rather than living with the fake.
curl -sSfL -o "$SRC/AdwaitaSans-Italic.ttf" "$ADWAITA_RAW/sans/AdwaitaSans-Italic.ttf"
instance "$SRC/AdwaitaSans-Italic.ttf" wght=400 opsz=14 -o "$SRC/AdwaitaSans-Italic-static.ttf"
subset "$SRC/AdwaitaSans-Italic-static.ttf" \
  --output-file="$OUT/AdwaitaSans-Italic.subset.woff2" \
  --flavor=woff2 --unicodes="$UNICODES" \
  --layout-features="$FEATURES" --name-IDs='*'

# -- Adwaita Mono (data) ------------------------------------------------------
# Regular only: nothing in this UI sets a weight on a mono element, and the
# upstream file is 1.4 MB of near-universal coverage before subsetting.
curl -sSfL -o "$SRC/AdwaitaMono-Regular.ttf" "$ADWAITA_RAW/mono/AdwaitaMono-Regular.ttf"
subset "$SRC/AdwaitaMono-Regular.ttf" \
  --output-file="$OUT/AdwaitaMono-Regular.subset.woff2" \
  --flavor=woff2 --unicodes="$UNICODES" \
  --layout-features="$FEATURES" --name-IDs='*'

# -- iA Writer Duo (display) --------------------------------------------------
# Converted, never subset -- see the licensing note at the top. `--unicodes=*`
# plus `--layout-features=*` is how you say "change the container and nothing
# else" to pyftsubset.
curl -sSfL -o "$SRC/iAWriterDuoV.ttf" "$IA_RAW/iA%20Writer%20Duo/Variable/iAWriterDuoV.ttf"
subset "$SRC/iAWriterDuoV.ttf" \
  --output-file="$OUT/iAWriterDuoV.woff2" \
  --flavor=woff2 --unicodes='*' \
  --layout-features='*' --name-IDs='*' --glyph-names --notdef-outline

# -- licences -----------------------------------------------------------------
curl -sSfL -o "$OUT/OFL-Adwaita.txt" "$ADWAITA_RAW/LICENSE"
curl -sSfL -o "$OUT/OFL-iAWriter.txt" "$IA_RAW/iA%20Writer%20Duo/LICENSE.md"

echo
echo "wrote:"
ls -l "$OUT" | awk 'NR>1 {printf "  %-40s %8d bytes\n", $9, $5}'
