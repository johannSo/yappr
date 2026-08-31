#!/usr/bin/env python3
"""Finetune S1-mini for German dictation normalization. Target: one RTX 3090.

    pip install "torch" "transformers>=4.51" "trl>=0.12" "datasets" "accelerate"
    python3 build_dataset.py
    python3 train_s1_de.py

Full finetune, not LoRA. The model is 596M params; bf16 weights + fp32 AdamW
plus the logits term lands around 14 GB of the 3090's 24 GB. On a GTX 1080
this script will NOT work as written: Pascal has no bf16. See the notes at the
bottom of this file.

The single biggest memory term is not the model, it is the cross-entropy
logits: vocab is 151936, so batch*seq*151936*4 bytes*2. Keep
per_device_train_batch_size low and reach the effective batch with
gradient accumulation.
"""
import json
import pathlib

import torch
from datasets import Dataset
from transformers import AutoModelForCausalLM, AutoTokenizer
from trl import SFTConfig, SFTTrainer

HERE = pathlib.Path(__file__).resolve().parent

BASE = "superwhisper/s1-mini"
# The upstream weights were replaced in place twice ("Add s1-mini v075",
# "Update to v076") and the advertised `v1` tag does not exist. Pin the sha.
REVISION = "88f6b15896c73bbb13a3b596e0afe8ea0d5150b4"
OUT = HERE / "s1-mini-de"


def load(split: str) -> Dataset:
    path = HERE / f"{split}.jsonl"
    rows = [json.loads(l) for l in path.read_text(encoding="utf-8").splitlines() if l.strip()]
    return Dataset.from_list(rows)


def main() -> None:
    train_ds, val_ds = load("train"), load("val")
    print(f"train {len(train_ds)}  val {len(val_ds)}")

    tok = AutoTokenizer.from_pretrained(BASE, revision=REVISION)
    model = AutoModelForCausalLM.from_pretrained(
        BASE,
        revision=REVISION,
        dtype=torch.bfloat16,
        attn_implementation="sdpa",  # not flash_attention_2: no gain at 596M, one more build
    )

    cfg = SFTConfig(
        output_dir=str(OUT),
        # Low LR on purpose. S1-mini is already a finetuned specialist; a
        # typical 2e-4 destroys the control-line behaviour before it teaches
        # any German.
        learning_rate=1e-5,
        lr_scheduler_type="cosine",
        # ~3% of the ~130 total optimizer steps (1378 rows / effective
        # batch 32 * 3 epochs). An int count -- the installed TRL's SFTConfig
        # rejects `warmup_ratio` (TypeError at init), so don't switch back.
        warmup_steps=5,
        num_train_epochs=3,
        per_device_train_batch_size=4,
        gradient_accumulation_steps=8,   # effective batch 32
        max_length=1024,                 # inference runs n_ctx=2048; this fits
        packing=False,                   # never pack: it would merge utterances
        bf16=True,
        logging_steps=5,
        eval_strategy="epoch",
        save_strategy="epoch",
        save_total_limit=2,
        report_to=[],
    )

    trainer = SFTTrainer(model=model, args=cfg, train_dataset=train_ds, eval_dataset=val_ds, processing_class=tok)

    # --- self-check: look at what is actually being trained on -------------
    # Verifies three things that silently ruin this finetune if wrong:
    #   1. exactly one <|im_end|> at the end (no double stop),
    #   2. the pre-closed <think></think> block survived tokenization,
    #   3. the loss is masked to the completion only (prompt labels == -100).
    batch = next(iter(trainer.get_train_dataloader()))
    ids = batch["input_ids"][0]
    labels = batch["labels"][0]
    text = tok.decode(ids, skip_special_tokens=False)
    print("\n--- first training example, as the model sees it ---")
    print(text)
    n_end = text.count("<|im_end|>")
    supervised = tok.decode([i for i, l in zip(ids, labels) if l != -100], skip_special_tokens=False)
    print(f"\n--- supervised span (loss is computed on this only) ---\n{supervised}")
    assert "<think>\n\n</think>" in text, "the pre-closed think block is missing"
    assert n_end == 3, f"expected 3 <|im_end|> (system, user, completion), got {n_end}"
    assert (labels == -100).any(), "nothing is masked; the prompt is being trained on"
    print("\nself-check passed\n")
    # ----------------------------------------------------------------------

    trainer.train()
    trainer.save_model(str(OUT))
    tok.save_pretrained(str(OUT))
    print(f"\nsaved to {OUT}")
    print("""
Next:
  python3 export_gguf.py --smoke

That clones and builds llama.cpp, converts s1-mini-de/ to GGUF, quantizes it to
Q4_K_M, and prints the sha256 plus the scp/--update-lock commands for the
machine that runs yappr.
""")


# --- GTX 1080 (Pascal, sm_61) --------------------------------------------
# Pascal has no bf16 and CUDA 13 dropped the architecture entirely. To run
# this on a 1080, install torch from the cu126 index and change:
#   dtype=torch.float32, attn_implementation="sdpa"
#   SFTConfig(fp16=True, bf16=False, optim="adamw_bnb_8bit",
#             gradient_checkpointing=True, per_device_train_batch_size=1,
#             gradient_accumulation_steps=32, torch_compile=False, tf32=False)
# and wrap the model in a peft LoRA (r=16). Do not use Unsloth or flash-attn.
# Note `is_torch_bf16_gpu_available()` returns True on a 1080 via emulation,
# so bf16=True would be accepted and then run emulated and slow.

if __name__ == "__main__":
    main()
