#!/usr/bin/env python3
"""Build the S1-mini German finetuning set from validated part-*.jsonl files.

Emits prompt/completion pairs rendered byte-identically to what yappr sends at
inference time. The system prompt is read out of the Rust source rather than
retyped, so it cannot drift from `normalize::SYSTEM_PROMPT`, which is itself
pinned by a test against the model card.

    python3 build_dataset.py            # writes train.jsonl / val.jsonl

Train with TRL, which masks the loss to the completion automatically for a
prompt-completion dataset:

    SFTConfig(..., max_length=1024, packing=False)
"""
import json
import pathlib
import random
import re
import sys

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parents[1]
NORMALIZE_RS = ROOT / "crates" / "yappr-core" / "src" / "normalize.rs"

VAL_FRACTION = 0.10
SEED = 20260831

# Oversampling per part, applied to the TRAIN side only, after the val split.
#
# Why this exists: the v1 finetune (1e-5, 3 epochs) taught everything that
# agrees with the base model's prior and merely memorised everything that has
# to override it -- v2 answers its own part-g rows correctly (except clock
# digits!) but says "78 Prozent" for an unseen "siebenundachtzig". Digit and
# pronoun tokens are a rounding error in the average loss over 888 rows; these
# weights raise their gradient share without touching the learning rate that
# protects the control-line behaviour. Weights multiply rows verbatim, so keep
# them small; val stays unweighted and duplicate-free.
PART_WEIGHTS = {
    "part-g-numbers": 4,
    "part-l-numbers-deep": 4,
    "part-i-formal-sie": 3,
    # x3 made v3 over-list: 3-item chains that v2 handled as prose came back
    # bulleted and duplicated. x2 keeps the digit pressure without that.
    "part-n-residuals": 2,
    "part-o-v3-residuals": 2,
}


def system_prompt() -> str:
    """The exact bytes of `normalize::SYSTEM_PROMPT`."""
    src = NORMALIZE_RS.read_text(encoding="utf-8")
    m = re.search(r'pub const SYSTEM_PROMPT: &str = "(.*?)";', src, re.S)
    if not m:
        sys.exit(f"could not find SYSTEM_PROMPT in {NORMALIZE_RS}")
    return m.group(1)


def control_line(row: dict) -> str:
    """Mirrors `style::control_line`."""
    return (
        f"[Styling: {row['styling']}] "
        f"[Structure: {row['structure']}] "
        f"[Context: {row['context']}]"
    )


def render(row: dict, sys_prompt: str) -> dict:
    """Mirrors `normalize::render_chat_prompt`, split at the generation point.

    The pre-closed, empty <think> block is load-bearing: it is the
    `enable_thinking = false` branch of the model's own chat template. Without
    it S1-mini emits a reasoning trace, which the guardrail then rejects as
    template bleed.
    """
    prompt = (
        f"<|im_start|>system\n{sys_prompt}<|im_end|>\n"
        f"<|im_start|>user\n{control_line(row)}\n{row['raw']}<|im_end|>\n"
        f"<|im_start|>assistant\n<think>\n\n</think>\n\n"
    )
    # The completion deliberately does NOT carry a trailing <|im_end|>.
    # TRL appends `tokenizer.eos_token` to the completion of a
    # prompt-completion dataset, and for this model that IS <|im_end|>
    # (eos_token_id 151645). Writing it here too would train a double stop.
    # `train_s1_de.py` decodes one batch and asserts exactly one.
    return {"prompt": prompt, "completion": row["cleaned"]}


def main() -> None:
    parts = sorted(HERE.glob("part-*.jsonl"))
    if not parts:
        sys.exit("no part-*.jsonl files found")

    # The probes in probes/ are the eval set: a probe whose raw appears in the
    # training data stops measuring generalisation and starts measuring
    # memorisation. This happened once (nine part-h rows were pasted straight
    # from the probe battery), so the check is mechanical now.
    probes_file = HERE / "probes" / "cases.json"
    probe_raws = set()
    if probes_file.exists():
        doc = json.loads(probes_file.read_text(encoding="utf-8"))
        probe_raws = {c["raw"] for c in doc["cases"]}

    rows, seen, dupes = [], set(), 0
    for p in parts:
        for line in p.read_text(encoding="utf-8").splitlines():
            if not line.strip():
                continue
            row = json.loads(line)
            if row["raw"] in probe_raws:
                sys.exit(f"{p.name}: raw is a probe from probes/cases.json - "
                         f"reword the training row, never the probe:\n  {row['raw']}")
            if row["raw"] in seen:
                dupes += 1
                continue
            seen.add(row["raw"])
            row["_part"] = p.stem
            rows.append(row)

    combos = {}
    for r in rows:
        combos[control_line(r)] = combos.get(control_line(r), 0) + 1

    rng = random.Random(SEED)
    rng.shuffle(rows)
    n_val = max(1, int(len(rows) * VAL_FRACTION))
    val, train = rows[:n_val], rows[n_val:]

    def weight(stem: str) -> int:
        return next((w for k, w in PART_WEIGHTS.items() if stem.startswith(k)), 1)

    train = [r for r in train for _ in range(weight(r["_part"]))]
    rng.shuffle(train)

    sp = system_prompt()
    for name, subset in (("train", train), ("val", val)):
        out = HERE / f"{name}.jsonl"
        with out.open("w", encoding="utf-8") as f:
            for r in subset:
                f.write(json.dumps(render(r, sp), ensure_ascii=False) + "\n")
        print(f"{out.name:12} {len(subset):5} rows")

    from collections import Counter
    eff = Counter(r["_part"] for r in train)
    print("\ntrain-Gewichtung (effektive Zeilen je Part):")
    for stem in sorted(eff):
        w = weight(stem)
        note = f"  x{w}" if w > 1 else ""
        print(f"  {eff[stem]:5}  {stem}{note}")

    print(f"\n{len(rows)} unique rows from {len(parts)} parts ({dupes} duplicates dropped)")
    print(f"control-line coverage: {len(combos)}/16")
    for k in sorted(combos):
        print(f"  {combos[k]:4}  {k}")

    lens = sorted(len(r["raw"]) + len(r["cleaned"]) for r in rows)
    print(f"\nchars per example: median {lens[len(lens)//2]}, p95 {lens[int(len(lens)*0.95)]}, max {lens[-1]}")
    print("(divide by ~3.0 for a German token estimate; max_length=1024 is ample)")


if __name__ == "__main__":
    main()
