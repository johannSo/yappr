#!/usr/bin/env bash
# Upload the primeline sherpa-onnx export produced by
# scripts/export-primeline-onnx.sh, and print the catalogue entry to paste.
#
# Hosting is ours rather than a third party's on purpose: this model is loaded
# and run locally on every dictation, and the one pre-existing primeline export
# on the Hub declares no license, ships unrelated diarization models alongside,
# and stamps a provenance URL for a different checkpoint. primeline-parakeet is
# CC-BY-4.0, which permits redistribution with attribution -- so the honest
# option is to export it ourselves and say where it came from.
#
# Usage: scripts/publish-primeline-onnx.sh <tarball> [hf-namespace]

set -euo pipefail

TARBALL="${1:?usage: publish-primeline-onnx.sh <tarball> [hf-namespace]}"
NAMESPACE="${2:-Joni000000000}"
REPO_NAME="parakeet-primeline-sherpa-onnx-int8"
REPO="$NAMESPACE/$REPO_NAME"

die() { printf 'error: %s\n' "$*" >&2; exit 1; }
log() { printf '\n=== %s\n' "$*"; }

[ -f "$TARBALL" ] || die "no such file: $TARBALL"
command -v hf >/dev/null || command -v huggingface-cli >/dev/null \
  || die "no hf CLI; pip install -U huggingface_hub"
HF=$(command -v hf || command -v huggingface-cli)

"$HF" auth whoami >/dev/null 2>&1 || die "not logged in; run: $HF auth login"

BASENAME=$(basename "$TARBALL")

# Re-derive the pin from the tarball itself rather than trusting a number
# copied by hand out of the export script's output.
log "deriving the pin from the tarball"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
tar xjf "$TARBALL" -C "$TMP"
ENCODER=$(find "$TMP" -name encoder.int8.onnx -print -quit)
[ -n "$ENCODER" ] || die "no encoder.int8.onnx inside $TARBALL"
SHA=$(sha256sum "$ENCODER" | cut -d' ' -f1)
echo "  $SHA"

log "creating $REPO if it does not exist"
"$HF" repo create "$REPO" --repo-type model -y 2>/dev/null || true

cat > "$TMP/README.md" <<'EOF'
---
license: cc-by-4.0
language:
- de
pipeline_tag: automatic-speech-recognition
tags:
- sherpa-onnx
- parakeet
- onnx
---

# parakeet-primeline, sherpa-onnx export (int8)

A sherpa-onnx-compatible ONNX export of
[primeline/parakeet-primeline](https://huggingface.co/primeline/parakeet-primeline),
which is itself a German finetune of
[nvidia/parakeet-tdt-0.6b-v3](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3).

Produced with sherpa-onnx's own first-party export script
(`scripts/nemo/parakeet-tdt-0.6b-v3/export_onnx.py`), unmodified except for the
provenance strings in the ONNX metadata, which upstream hardcodes to nvidia's
URL. The exporting wrapper is `scripts/export-primeline-onnx.sh` in the yappr
repository.

Contents: `encoder.int8.onnx`, `decoder.int8.onnx`, `joiner.int8.onnx`,
`tokens.txt` (and `bpe.vocab`, for sherpa's hotword support). Offline
transducer; load it with sherpa-onnx's `OfflineRecognizer` and
`model_type = "nemo_transducer"`.

Licensed CC-BY-4.0, inherited from the upstream model. Original work by
primeline; this repository redistributes a format conversion of it.
EOF

log "uploading"
"$HF" upload "$REPO" "$TARBALL" "$BASENAME" --repo-type model
"$HF" upload "$REPO" "$TMP/README.md" README.md --repo-type model

URL="https://huggingface.co/$REPO/resolve/main/$BASENAME"

log "done"
cat <<EOF

Add to crates/yappr-core/models.lock.toml (keys stay sorted):

  parakeet-primeline-de = "$SHA"

Add to ASR_MODELS in crates/yappr-core/src/models.rs, and bump the array
length from 3 to 4:

    AsrModelSpec {
        key: AsrModel::ParakeetPrimelineDe,
        artifact: Artifact {
            name: "parakeet-primeline-de",
            url: "$URL",
            display: "primeline Parakeet 0.6b (int8)",
            rel_path: "parakeet-primeline-de-int8",
            archive: true,
        },
        flavor: AsrFlavor::Offline,
        display: "primeline Parakeet 0.6b — nur Deutsch, genaueste deutsche Erkennung",
    },

The rest is Task 12 of docs/superpowers/plans/2026-09-08-asr-model-selection.md:
the AsrModel variant, the two test loops, ENUMS in src/settings/schema.ts, and
the ASR_MODELS option list in src/settings/wizard.tsx.

EOF
