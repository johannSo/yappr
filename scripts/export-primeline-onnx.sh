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
avail_kb=$(df -Pk "$WORKDIR" | awk 'NR==2 {print $4}')
if [ "$avail_kb" -lt 8388608 ]; then
  die "need ~8 GB free in $WORKDIR, have $((avail_kb / 1024)) MB.
     Peak usage is the 2.51 GB checkpoint plus a ~2.6 GB fp32 intermediate
     that exists until quantisation collapses it to ~650 MB."
fi

cd "$WORKDIR"
log "workdir: $WORKDIR  ($((avail_kb / 1048576)) GB free)"

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
log "fetching upstream export scripts"
mkdir -p nemo/parakeet-tdt-0.6b-v3
curl -fsSL -o nemo/generate_bpe_vocab.py "$SHERPA_RAW/generate_bpe_vocab.py"
curl -fsSL -o nemo/parakeet-tdt-0.6b-v3/export_onnx.py "$SHERPA_RAW/parakeet-tdt-0.6b-v3/export_onnx.py"
curl -fsSL -o nemo/parakeet-tdt-0.6b-v3/test_onnx.py "$SHERPA_RAW/parakeet-tdt-0.6b-v3/test_onnx.py"

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

log "transcribing de.wav with the int8 export"
python nemo/parakeet-tdt-0.6b-v3/test_onnx.py \
  --encoder ./encoder.int8.onnx \
  --decoder ./decoder.int8.onnx \
  --joiner ./joiner.int8.onnx \
  --tokens ./tokens.txt \
  --wav ./de.wav

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

Next: scripts/publish-primeline-onnx.sh "$WORKDIR/$REL_PATH.tar.bz2" <hf-namespace>

Then add to crates/yappr-core/models.lock.toml, keys sorted:

  parakeet-primeline-de = "$SHA"

EOF
