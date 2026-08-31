#!/usr/bin/env python3
"""Run the failure-class probes against a model and record what it answers.

The probes are the eval half of `data/finetune`: 64 inputs, each one an
instance of a failure class observed on a real build, grouped by class in
`cases.json`. Nothing here is training data, and nothing here appears in a
`part-*.jsonl` file -- a probe the model was trained on stops measuring
anything.

    llama-server -m <model>.gguf --port 8899 -c 4096
    python3 probes/run.py --out before.json          # current model
    #   ... retrain, export, restart llama-server ...
    python3 probes/run.py --out after.json
    python3 probes/run.py --diff before.json after.json

The prompt is rendered byte-identically to `normalize::render_chat_prompt`
and sampled greedily, which is what `LlamaEngine::generate` does -- so a run
is deterministic and two runs of the same model agree exactly. A difference
between `before` and `after` is the training talking, not the sampler.
"""
import argparse
import json
import pathlib
import sys
import urllib.request

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parents[2]
NORMALIZE_RS = ROOT / "crates" / "yappr-core" / "src" / "normalize.rs"


def system_prompt() -> str:
    """The exact bytes of `normalize::SYSTEM_PROMPT`, read from the source.

    Same trick as `build_dataset.py`: retyping it here would let the probes
    drift away from what the app actually sends, and a probe that measures a
    different prompt measures nothing.
    """
    import re
    src = NORMALIZE_RS.read_text(encoding="utf-8")
    m = re.search(r'pub const SYSTEM_PROMPT: &str = "(.*?)";', src, re.S)
    if not m:
        sys.exit(f"could not find SYSTEM_PROMPT in {NORMALIZE_RS}")
    return m.group(1)


def generate(case: dict, sys_prompt: str, url: str) -> str:
    control = (f"[Styling: {case['styling']}] [Structure: {case['structure']}] "
               f"[Context: {case['context']}]")
    prompt = (f"<|im_start|>system\n{sys_prompt}<|im_end|>\n"
              f"<|im_start|>user\n{control}\n{case['raw']}<|im_end|>\n"
              f"<|im_start|>assistant\n<think>\n\n</think>\n\n")
    body = json.dumps({
        "prompt": prompt,
        "temperature": 0,      # `LlamaSampler::greedy()` in llama.rs
        "top_k": 1,
        "n_predict": 512,
        "stop": ["<|im_end|>"],
        "cache_prompt": False,
    }).encode()
    req = urllib.request.Request(url, body, {"Content-Type": "application/json"})
    return json.loads(urllib.request.urlopen(req, timeout=300).read())["content"]


def load_cases() -> tuple[dict, list]:
    doc = json.loads((HERE / "cases.json").read_text(encoding="utf-8"))
    return doc["legend"], doc["cases"]


def cmd_run(args) -> None:
    legend, cases = load_cases()
    sp = system_prompt()
    results = {}
    for c in cases:
        out = generate(c, sp, args.url)
        results[c["id"]] = out
        print("=" * 78)
        print(f"{c['id']:10} [{c['class']}] {legend[c['class']]}")
        print("RAW  ", c["raw"])
        print("OUT  ", out.replace("\n", "\n      "))
    if args.out:
        pathlib.Path(args.out).write_text(
            json.dumps(results, ensure_ascii=False, indent=1), encoding="utf-8")
        print(f"\n{len(results)} Antworten -> {args.out}")


def cmd_diff(args) -> None:
    legend, cases = load_cases()
    before = json.loads(pathlib.Path(args.diff[0]).read_text(encoding="utf-8"))
    after = json.loads(pathlib.Path(args.diff[1]).read_text(encoding="utf-8"))
    changed = 0
    for c in cases:
        b, a = before.get(c["id"]), after.get(c["id"])
        if b is None or a is None or b == a:
            continue
        changed += 1
        print("=" * 78)
        print(f"{c['id']:10} [{c['class']}] {legend[c['class']]}")
        print("RAW    ", c["raw"])
        print("VORHER ", b.replace("\n", "\n        "))
        print("NACHHER", a.replace("\n", "\n        "))
    print(f"\n{changed} von {len(cases)} Sonden haben sich geändert.")


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--url", default="http://localhost:8899/completion",
                   help="llama-server completion endpoint")
    p.add_argument("--out", help="write the answers to this JSON file")
    p.add_argument("--diff", nargs=2, metavar=("VORHER", "NACHHER"),
                   help="compare two saved runs instead of calling the model")
    args = p.parse_args()
    (cmd_diff if args.diff else cmd_run)(args)


if __name__ == "__main__":
    main()
