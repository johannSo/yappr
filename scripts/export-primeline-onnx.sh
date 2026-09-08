#!/usr/bin/env bash
# Produce a sherpa-onnx export of primeline/parakeet-primeline, packaged the
# way `models::download_all` expects.
#
# Why this exists: primeline-parakeet is the most accurate German ASR model in
# yappr's catalogue (2.95 % vs the shipped Parakeet v3's 3.64 % average WER,
# 4.11 vs 7.05 on Tuda-De), but upstream publishes only a `.nemo` checkpoint,
# which sherpa-onnx cannot load. Every other model in `ASR_MODELS` comes from
# k2-fsa's own `asr-models` release; this one has to be exported.
#
# It is a German finetune of nvidia/parakeet-tdt-0.6b-v3 -- the same
# FastConformer-TDT architecture yappr already ships -- so sherpa-onnx's own
# first-party export script for that model applies unmodified. This wrapper
# only arranges the inputs, corrects the provenance metadata, and packages the
# result; the export itself is upstream's code, fetched at run time.
#
# The GPU is irrelevant. ONNX export is a tracing pass (~1 s), and the
# dominant cost -- `quantize_dynamic` -- is CPU-only in ONNX Runtime with no
# CUDA path. CPU-only torch is therefore installed deliberately: it is a few
# hundred MB against ~2.5 GB for the CUDA wheel plus several GB of nvidia-*
# transitive dependencies, which would be pure overhead here.
#
# Usage:  scripts/export-primeline-onnx.sh [workdir]
# Output: <workdir>/parakeet-primeline-de-int8.tar.bz2 and its sha256.
#
# Requires: Python 3.10-3.12 (NeMo does not support 3.13+), ~8 GB free disk,
# and network access. Expect 25-45 minutes cold.

set -euo pipefail

WORKDIR="${1:-$PWD/primeline-export}"
# The directory name inside the tarball. Must match `Artifact::rel_path` in
# crates/yappr-core/src/models.rs, and `extract_tar_bz2` flattens exactly one
# top-level directory -- so the tarball must contain this and nothing beside it.
REL_PATH="parakeet-primeline-de-int8"
# What sherpa's v3 export script looks for next to itself. It prefers a local
# checkpoint over downloading nvidia's, which is the whole trick: point that
# name at primeline's weights and the script exports those instead.
NEMO_LOCAL_NAME="parakeet-tdt-0.6b-v3.nemo"
NEMO_URL="https://huggingface.co/primeline/parakeet-primeline/resolve/main/2_95_WER.nemo"
SHERPA_RAW="https://raw.githubusercontent.com/k2-fsa/sherpa-onnx/master/scripts/nemo"
WAV_URL="https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/de.wav"

TRANSCRIBE_OK=1

log() { printf '\n=== %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------- preflight

PYBIN="${PYTHON:-python3}"
command -v "$PYBIN" >/dev/null || die "no $PYBIN on PATH"
"$PYBIN" - <<'PY' || die "NeMo needs Python 3.10-3.12; set PYTHON=/path/to/python3.12"
import sys
major, minor = sys.version_info[:2]
sys.exit(0 if (major == 3 and 10 <= minor <= 12) else 1)
PY

mkdir -p "$WORKDIR"
cd "$WORKDIR"

# Whether the expensive half still has to run decides how much disk is
# needed, so answer that first. Re-running after a late failure must not be
# blocked by a requirement only the export itself has.
if [ -s encoder.int8.onnx ] && [ -s decoder.int8.onnx ] \
   && [ -s joiner.int8.onnx ] && [ -s tokens.txt ]; then
  EXPORT_NEEDED=0
  need_kb=$((2 * 1024 * 1024))   # packaging: a copy of the int8 files, plus the tarball
  need_human="~2 GB"
else
  EXPORT_NEEDED=1
  need_kb=$((8 * 1024 * 1024))
  need_human="~8 GB"
fi

avail_kb=$(df -Pk . | awk 'NR==2 {print $4}')
if [ "$avail_kb" -lt "$need_kb" ]; then
  die "need $need_human free in $WORKDIR, have $((avail_kb / 1024)) MB.
     $( [ "$EXPORT_NEEDED" -eq 1 ] \
        && printf '%s' 'Peak is the 2.51 GB checkpoint plus a ~2.6 GB fp32 intermediate that exists until quantisation collapses it to ~650 MB.' \
        || printf '%s' 'Only packaging is left: a copy of the ~640 MB of int8 files, plus the tarball.' )"
fi

log "workdir: $WORKDIR  ($((avail_kb / 1048576)) GB free, export needed: $EXPORT_NEEDED)"

# ------------------------------------------------------------------- python

if [ ! -d venv ]; then
  log "creating venv"
  "$PYBIN" -m venv venv
fi
# shellcheck disable=SC1091
. venv/bin/activate

if ! python -c "import nemo.collections.asr" 2>/dev/null; then
  log "installing CPU-only torch (the GPU cannot help; see header)"
  pip install --quiet --upgrade pip
  pip install --quiet torch torchaudio --index-url https://download.pytorch.org/whl/cpu

  log "installing NeMo and the export toolchain"
  # Versions are upstream's own (scripts/nemo/parakeet-tdt-0.6b-v3/run.sh).
  # numpy<2 is not optional: NeMo's ASR stack still breaks on numpy 2.
  pip install --quiet \
    "nemo_toolkit[asr]" \
    "numpy<2" \
    ipython \
    kaldi-native-fbank \
    librosa \
    "onnx==1.17.0" \
    "onnxruntime==1.17.1" \
    soundfile

  # NeMo's resolver may drag these forward again; upstream's pins are what the
  # export script is known to work against, so restate them last.
  pip install --quiet --force-reinstall --no-deps \
    "numpy<2" "onnx==1.17.0" "onnxruntime==1.17.1"
fi

# ------------------------------------------------------------------- inputs

if [ ! -f "$NEMO_LOCAL_NAME" ]; then
  log "downloading primeline checkpoint (2.51 GB, resumable)"
  curl -fL --retry 3 -C - -o "$NEMO_LOCAL_NAME.part" "$NEMO_URL"
  mv "$NEMO_LOCAL_NAME.part" "$NEMO_LOCAL_NAME"
fi

[ -f de.wav ] || curl -fsSL -o de.wav "$WAV_URL"

# Upstream's script imports a sibling module by walking one directory up, so
# the on-disk layout has to match the repo's.
#
# `fetch_script` exists because GitHub's raw endpoint serves a *symlinked*
# file as its target path in plain text rather than as the file's contents.
# scripts/nemo/parakeet-tdt-0.6b-v3/test_onnx.py is a 36-byte symlink to the
# v2 copy, so fetching it naively yields a one-line file that Python then
# rejects with "SyntaxError: invalid decimal literal". Resolved generically
# rather than by hardcoding the v2 path, because any of these files could
# become a symlink later.
fetch_script() {
  local rel="$1" dest="$2" hops=0 target resolved
  while :; do
    curl -fsSL -o "$dest" "$SHERPA_RAW/$rel"
    # A symlink payload is a single short line holding a relative path.
    if [ "$(wc -c <"$dest")" -ge 256 ] || [ "$(wc -l <"$dest")" -gt 1 ]; then
      return 0
    fi
    target=$(tr -d '\n' <"$dest")
    case "$target" in
      ./*|../*|*/*) ;;
      *) return 0 ;;   # not a path: a genuinely tiny script
    esac
    hops=$((hops + 1))
    [ "$hops" -le 4 ] || die "symlink loop resolving $rel"
    resolved=$(realpath -m --relative-to=/r "/r/$(dirname "$rel")/$target")
    printf '  %s -> %s\n' "$rel" "$resolved"
    rel="$resolved"
  done
}

# Fetching a script that is not valid Python has to be reported as that,
# naming the file and what it actually contains. Otherwise it surfaces
# minutes later as a bare SyntaxError pointing at line 1 of a file nobody
# has looked at, which says nothing about where the bad content came from.
assert_python() {
  local f="$1"
  python -c "import ast,sys; ast.parse(open(sys.argv[1]).read())" "$f" 2>/dev/null && return 0
  printf 'error: %s is not valid Python (%s bytes). First line:\n  %s\n' \
    "$f" "$(wc -c <"$f")" "$(head -1 "$f")" >&2
  return 1
}

log "fetching upstream export scripts"
mkdir -p nemo/parakeet-tdt-0.6b-v3
fetch_script generate_bpe_vocab.py nemo/generate_bpe_vocab.py
fetch_script parakeet-tdt-0.6b-v3/export_onnx.py nemo/parakeet-tdt-0.6b-v3/export_onnx.py
fetch_script parakeet-tdt-0.6b-v3/test_onnx.py nemo/parakeet-tdt-0.6b-v3/test_onnx.py

# export_onnx.py is load-bearing, so a bad fetch must stop the run. Checked
# up front rather than after the download and the venv build.
assert_python nemo/generate_bpe_vocab.py || die "refusing to run a broken export script"
assert_python nemo/parakeet-tdt-0.6b-v3/export_onnx.py || die "refusing to run a broken export script"

# Everything above is cheap and idempotent. The export is neither -- it is
# 10-25 minutes of CPU -- so a re-run picks up from whatever is already on
# disk. Delete the int8 files to force it again.
if [ "$EXPORT_NEEDED" -eq 0 ]; then
  log "int8 export already present -- skipping export, going straight to verification"
else

# Correct the provenance stamped into the ONNX metadata. Upstream hardcodes
# nvidia's URL, and an export of a *different* checkpoint carrying that string
# is actively misleading -- the one pre-existing third-party primeline export
# on the Hub says "parakeet-tdt-0.6b-v2" for exactly this reason, which makes
# it impossible to tell what it actually contains. Fails loudly rather than
# silently mis-stamping if upstream ever rewords this.
log "patching export metadata to name primeline"
python - <<'PY'
from pathlib import Path

p = Path("nemo/parakeet-tdt-0.6b-v3/export_onnx.py")
s = p.read_text()

old_url = '"url": "https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3",'
old_comment = '"comment": "Only the transducer branch is exported",'
assert old_url in s, "upstream changed the url metadata line; re-check the patch"
assert old_comment in s, "upstream changed the comment metadata line; re-check the patch"

s = s.replace(
    old_url,
    '"url": "https://huggingface.co/primeline/parakeet-primeline",',
)
s = s.replace(
    old_comment,
    '"comment": "primeline-parakeet, a German finetune of '
    'nvidia/parakeet-tdt-0.6b-v3. Only the transducer branch is exported.",',
)
p.write_text(s)
print("  metadata now names primeline")
PY

# ------------------------------------------------------------------- export

log "exporting to ONNX and quantising (the long part: 10-25 min)"
python nemo/parakeet-tdt-0.6b-v3/export_onnx.py

for f in encoder.int8.onnx decoder.int8.onnx joiner.int8.onnx tokens.txt; do
  [ -s "$f" ] || die "$f was not produced"
done

# The fp32 encoder is over 2 GB, so torch.onnx.export writes its weights as
# hundreds of loose external-data files ("onnx__MatMul_7060",
# "layers.3.conv.pointwise_conv1.weight", ...), and `add_meta_data` then
# re-saves a consolidated `encoder.weights` beside them. None of it is needed
# once the int8 files exist, and together it is ~5 GB -- enough to fill the
# disk and block the packaging step that follows.
#
# The delete list comes from the model itself rather than from a glob: those
# filenames have no extension and no safely distinctive prefix, and this
# directory also holds the outputs we must not touch.
log "removing fp32 intermediates"
python - <<'PY'
import os
from pathlib import Path

import onnx

removed = bytes_freed = 0
if Path("encoder.onnx").exists():
    model = onnx.load("encoder.onnx", load_external_data=False)
    targets = set()
    for init in model.graph.initializer:
        if init.data_location == onnx.TensorProto.EXTERNAL:
            for kv in init.external_data:
                if kv.key == "location":
                    targets.add(kv.value)
    for name in sorted(targets):
        f = Path(name)
        # Never step outside the work directory on a malformed location.
        if f.is_absolute() or ".." in f.parts or not f.is_file():
            continue
        bytes_freed += f.stat().st_size
        f.unlink()
        removed += 1

for name in ("encoder.onnx", "encoder.weights", "decoder.onnx", "joiner.onnx"):
    f = Path(name)
    if f.is_file():
        bytes_freed += f.stat().st_size
        f.unlink()
        removed += 1

print(f"  removed {removed} files, {bytes_freed / 1e9:.2f} GB")
PY

fi   # end of the skip-if-already-exported guard

# --------------------------------------------------------------- verify

log "checking the exported metadata"
python - <<'PY'
import onnx

m = onnx.load("encoder.int8.onnx", load_external_data=False)
meta = {p.key: p.value for p in m.metadata_props}
for k in ("feat_dim", "model_type", "vocab_size", "normalize_type", "url"):
    print(f"  {k:16} = {meta.get(k)}")

# feat_dim 128 is what `SherpaTranscriber` and its streaming sibling assume;
# a mismatch here yields confident wrong text rather than an error.
assert meta.get("feat_dim") == "128", f"unexpected feat_dim {meta.get('feat_dim')!r}"
assert meta.get("model_type") == "EncDecRNNTBPEModel", meta.get("model_type")
# v3 lineage carries an 8192-token vocabulary; the English-only v2 does not.
# This is the check that would have caught a wrong base checkpoint.
assert meta.get("vocab_size") == "8192", (
    f"vocab_size {meta.get('vocab_size')!r} is not v3 lineage -- wrong checkpoint?"
)
assert "primeline" in (meta.get("url") or ""), "provenance patch did not take"
print("  metadata OK")
PY

# Deliberately non-fatal, and this is the important part of the script's
# error handling.
#
# The hard gate is the metadata check above: feat_dim, model_type and a
# vocab_size that proves v3 lineage are what decide whether these files are
# usable at all, and they are computed from the export itself. This step is a
# convenience on top -- it runs a *third-party script fetched at run time*
# against a *downloaded wav*, so it has failure modes (a moved URL, a
# symlinked file, a missing codec) that say nothing whatsoever about the
# quality of the export.
#
# It previously ran under `set -e`, so any of those aborted the whole run
# *after* 20 minutes of successful quantisation and before packaging --
# throwing away the expensive, correct result over a broken sanity check.
# Now it warns and the tarball still gets built.
log "transcribing de.wav with the int8 export (optional check)"
if assert_python nemo/parakeet-tdt-0.6b-v3/test_onnx.py \
   && python nemo/parakeet-tdt-0.6b-v3/test_onnx.py \
        --encoder ./encoder.int8.onnx \
        --decoder ./decoder.int8.onnx \
        --joiner ./joiner.int8.onnx \
        --tokens ./tokens.txt \
        --wav ./de.wav; then
  :
else
  TRANSCRIBE_OK=0
  printf '\n  WARNING: the optional transcription check did not run.\n'
  printf '  The export itself passed its metadata checks and is packaged below.\n'
  printf '  Verify it in yappr instead, or re-run this check by hand.\n\n'
fi

# -------------------------------------------------------------- package

log "packaging"
rm -rf "$REL_PATH" "$REL_PATH.tar.bz2"
mkdir "$REL_PATH"
cp encoder.int8.onnx decoder.int8.onnx joiner.int8.onnx tokens.txt "$REL_PATH/"
# bpe.vocab is only needed for sherpa's hotword support, which yappr does not
# configure. Carried anyway: it costs ~100 KB and its absence would be
# awkward to fix later, since the tarball is pinned by hash.
[ -f bpe.vocab ] && cp bpe.vocab "$REL_PATH/"

tar cjf "$REL_PATH.tar.bz2" "$REL_PATH"

# `extract_tar_bz2` takes the single top-level directory and fails if there
# isn't exactly one, so prove the shape before anyone uploads it.
log "verifying tarball shape"
tops=$(tar tjf "$REL_PATH.tar.bz2" | cut -d/ -f1 | sort -u)
[ "$(printf '%s\n' "$tops" | wc -l)" -eq 1 ] \
  || die "tarball has more than one top-level entry:
$tops"
[ "$tops" = "$REL_PATH" ] || die "top-level directory is '$tops', expected '$REL_PATH'"

# The pin is taken over the extracted encoder, which is what `hash_target`
# hashes for an archive artifact -- not over the tarball.
SHA=$(sha256sum "$REL_PATH/encoder.int8.onnx" | cut -d' ' -f1)

log "done"
cat <<EOF

  tarball : $WORKDIR/$REL_PATH.tar.bz2  ($(du -h "$REL_PATH.tar.bz2" | cut -f1))
  pin     : $SHA
  checks  : metadata OK$( [ "$TRANSCRIBE_OK" -eq 1 ] \
              && printf '%s' ', transcription OK' \
              || printf '%s' ', transcription SKIPPED (see warning above)' )

Next: scripts/publish-primeline-onnx.sh "$WORKDIR/$REL_PATH.tar.bz2" <hf-namespace>

Then add to crates/yappr-core/models.lock.toml, keys sorted:

  parakeet-primeline-de = "$SHA"

EOF
