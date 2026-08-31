#!/usr/bin/env python3
"""Turn the finetuned `s1-mini-de/` into the GGUF yappr loads.

    python3 export_gguf.py                    # convert + quantize
    python3 export_gguf.py --smoke            # ...and check it answers German
    python3 export_gguf.py --quant Q5_K_M     # a bigger, closer quant

This replaces the three commands `train_s1_de.py` used to print, which assumed
a llama.cpp checkout that nothing had ever created.

**No root, and no compiler, is required.** Quantizing needs llama.cpp's
`llama-quantize`, and `--quantizer auto` gets it the cheapest way that works:

  1. one already sitting in the checkout's `build/bin`, or on PATH;
  2. else the official prebuilt `llama-<tag>-bin-ubuntu-x64.tar.gz` (16 MB).
     Measured on b10715: the binary and every .so it needs require at most
     GLIBC_2.34 / GLIBCXX_3.4.21, so this runs on anything from Ubuntu 22.04
     (glibc 2.35) onward. Needs no cmake, no g++, no root.
  3. else a local cmake build. `cmake` itself needs no root either -- there is
     a 30 MB manylinux wheel, and `ensure_cmake` pip-installs it if missing.
     This tier still needs a working C++ compiler, which is the one thing
     nothing here can conjure.

The conversion half is pure Python and needs the torch / transformers / numpy
you already installed to train. It deliberately does NOT install llama.cpp's
`requirements-convert_hf_to_gguf.txt` -- that file pins `torch==2.11.0` behind
`--extra-index-url .../whl/cpu`, so running it would replace the CUDA torch you
just trained with by a CPU build. `ensure_convert_deps` checks the three you
already have and installs only `sentencepiece`. The converter adds the
checkout's own `gguf-py` to `sys.path`, so `gguf` needs no install.
"""
import argparse
import hashlib
import json
import os
import pathlib
import platform
import shutil
import subprocess
import sys
import tarfile
import urllib.request

HERE = pathlib.Path(__file__).resolve().parent
LLAMA_CPP_URL = "https://github.com/ggml-org/llama.cpp"
RELEASES_API = "https://api.github.com/repos/ggml-org/llama.cpp/releases?per_page=10"

# yappr looks for exactly this name in ~/.local/share/yappr/models, whatever
# quant is inside it -- `llama.rs`'s `MODEL_FILE`.
YAPPR_MODEL_FILE = "s1-mini-q4_k_m.gguf"
YAPPR_MODELS_DIR = "~/.local/share/yappr/models"


class Tools:
    """Where `llama-quantize` and `llama-cli` are, and the env they need.

    A prebuilt tarball keeps its .so files next to the binaries rather than in
    a system path, so running one takes an LD_LIBRARY_PATH. A local cmake
    build links them by rpath and needs no env at all. Carrying the env here
    means callers never have to know which tier they got.
    """

    def __init__(self, quantize, cli, env, kind, root):
        self.quantize, self.cli, self.env, self.kind, self.root = quantize, cli, env, kind, root


def run(cmd, env=None, **kw) -> subprocess.CompletedProcess:
    print("+ " + " ".join(str(c) for c in cmd), flush=True)
    merged = {**os.environ, **env} if env else None
    return subprocess.run([str(c) for c in cmd], check=True, env=merged, **kw)


def need(tool: str) -> str:
    path = shutil.which(tool)
    if not path:
        sys.exit(f"{tool} is not on PATH; install it and re-run")
    return path


def ensure_checkout(root: pathlib.Path) -> pathlib.Path:
    """A shallow llama.cpp clone -- needed for convert_hf_to_gguf.py regardless
    of which quantizer tier is used. Any existing checkout is left alone."""
    if (root / "convert_hf_to_gguf.py").is_file():
        print(f"using existing llama.cpp checkout at {root}")
        return root
    if root.exists() and any(root.iterdir()):
        sys.exit(f"{root} exists but is not a llama.cpp checkout; move it or pass --llama-cpp")
    need("git")
    run(["git", "clone", "--depth", "1", LLAMA_CPP_URL, root])
    head = subprocess.run(["git", "-C", str(root), "rev-parse", "--short", "HEAD"],
                          capture_output=True, text=True).stdout.strip()
    print(f"cloned llama.cpp at {head}")
    return root


def ensure_convert_deps() -> None:
    """Make `convert_hf_to_gguf.py`'s imports resolve, without repinning torch.

    sentencepiece is needed even though this is a BPE tokenizer and the
    sentencepiece code never runs. `Qwen2Model.set_vocab` is:

        try:
            self._set_vocab_sentencepiece()
        except FileNotFoundError:
            self._set_vocab_gpt2()

    and `_set_vocab_gpt2` is the correct path for Qwen3 -- it is reached by
    `tokenizer.model` being absent from the model dir, which it always is.
    But `_create_vocab_sentencepiece` imports sentencepiece *before* it checks
    for that file, so with the module missing the raise is a ModuleNotFoundError,
    which that `except` does not catch. Installing it lets the import get far
    enough to fail in the way the fallback is written to expect.
    """
    import importlib
    import importlib.util

    for mod, why in (("numpy", "core dependency"),
                     ("torch", "reads the safetensors"),
                     ("transformers", "reads the tokenizer")):
        if importlib.util.find_spec(mod) is None:
            sys.exit(f"{mod} is missing from {sys.executable} ({why}).\n"
                     f"This is the environment that trained the model, so install it "
                     f"yourself rather than letting this script guess a version.")

    if importlib.util.find_spec("sentencepiece") is not None:
        return
    print("sentencepiece is missing; the Qwen vocab fallback needs it importable")
    run([sys.executable, "-m", "pip", "install", "sentencepiece"])
    # FileFinder caches site-packages listings, so a package installed during
    # this process is invisible to find_spec until the caches are dropped.
    importlib.invalidate_caches()
    if importlib.util.find_spec("sentencepiece") is None:
        sys.exit("sentencepiece still will not import after the install")


def existing_tools(root: pathlib.Path) -> "Tools | None":
    """Tier 1: a llama-quantize that is already here."""
    for d in (root / "build" / "bin", root / "build"):
        if (d / "llama-quantize").is_file():
            cli = d / "llama-cli"
            print(f"using existing {d / 'llama-quantize'}")
            return Tools(d / "llama-quantize", cli if cli.is_file() else None,
                         None, "build", root)
    on_path = shutil.which("llama-quantize")
    if on_path:
        print(f"using llama-quantize from PATH: {on_path}")
        return Tools(pathlib.Path(on_path), shutil.which("llama-cli"), None, "path", root)
    return None


def prebuilt_tools(root: pathlib.Path) -> Tools:
    """Tier 2: the official prebuilt binaries. No cmake, no compiler, no root."""
    if platform.machine() not in ("x86_64", "AMD64"):
        sys.exit(f"no prebuilt tier for {platform.machine()}; llama.cpp also ships "
                 f"an ubuntu-arm64 asset, but this script only wires up x64. "
                 f"Use --quantizer build.")
    dest = HERE / "llama-bin"
    found = next(iter(sorted(dest.glob("*/llama-quantize"))), None) if dest.is_dir() else None
    if found is None:
        with urllib.request.urlopen(RELEASES_API, timeout=30) as fh:
            releases = json.load(fh)
        # The GitHub "latest" release is not a build tag (it has been v0.3.0),
        # so pick the newest tag that actually looks like a build: b<number>.
        tag = next((r["tag_name"] for r in releases
                    if r["tag_name"].startswith("b") and r["tag_name"][1:].isdigit()), None)
        if tag is None:
            sys.exit("could not find a b<number> release tag; use --quantizer build")
        asset = f"llama-{tag}-bin-ubuntu-x64.tar.gz"
        url = f"{LLAMA_CPP_URL}/releases/download/{tag}/{asset}"
        dest.mkdir(parents=True, exist_ok=True)
        tarball = dest / asset
        print(f"downloading {url}")
        urllib.request.urlretrieve(url, tarball)
        # Refuse a tarball with absolute or parent-relative members rather
        # than letting extract write outside `dest`.
        with tarfile.open(tarball) as tf:
            for m in tf.getmembers():
                p = pathlib.PurePosixPath(m.name)
                if p.is_absolute() or ".." in p.parts:
                    sys.exit(f"refusing to extract unsafe path from {asset}: {m.name}")
            tf.extractall(dest)
        tarball.unlink()
        found = next(iter(sorted(dest.glob("*/llama-quantize"))), None)
        if found is None:
            sys.exit(f"{asset} contained no llama-quantize")
    bindir = found.parent
    for f in bindir.iterdir():                      # the tarball ships them non-executable
        if f.is_file() and (f.name.startswith("llama-") or f.name == "llama") and f.suffix == "":
            f.chmod(f.stat().st_mode | 0o111)
    cli = bindir / "llama-cli"
    print(f"using prebuilt {found}")
    return Tools(found, cli if cli.is_file() else None,
                 {"LD_LIBRARY_PATH": f"{bindir}:{os.environ.get('LD_LIBRARY_PATH', '')}".rstrip(":")},
                 "prebuilt", root)


def ensure_cmake() -> str:
    """cmake, pip-installed if absent. The wheel bundles the real binary, so
    this needs no root -- the usual blocker on a shared server."""
    found = shutil.which("cmake")
    if found:
        return found
    print("cmake is missing; installing the PyPI wheel (bundles the binary, no root needed)")
    run([sys.executable, "-m", "pip", "install", "cmake"])
    found = shutil.which("cmake") or str(pathlib.Path(sys.executable).with_name("cmake"))
    if not pathlib.Path(found).is_file():
        sys.exit("cmake still not found after the install; use --quantizer prebuilt")
    return found


def built_tools(root: pathlib.Path, targets=("llama-quantize",)) -> Tools:
    """Tier 3: build locally. The only tier that needs a C++ compiler."""
    cmake = ensure_cmake()
    if not any(shutil.which(c) for c in ("c++", "g++", "clang++")):
        sys.exit("no C++ compiler on PATH (tried c++, g++, clang++). "
                 "Nothing here can install one without root -- use --quantizer prebuilt.")
    run([cmake, "-S", root, "-B", root / "build",
         "-DCMAKE_BUILD_TYPE=Release",
         "-DLLAMA_CURL=OFF",   # otherwise cmake demands libcurl for a tool we don't build
         "-DGGML_CUDA=OFF"])   # quantizing is a CPU job; don't drag in the CUDA toolkit
    run([cmake, "--build", root / "build", "--target", *targets,
         "-j", str(os.cpu_count() or 4)])
    binary = root / "build" / "bin" / "llama-quantize"
    if not binary.is_file():
        sys.exit(f"the build finished but {binary} is missing")
    cli = root / "build" / "bin" / "llama-cli"
    return Tools(binary, cli if cli.is_file() else None, None, "build", root)


def get_tools(root: pathlib.Path, choice: str) -> Tools:
    if choice == "prebuilt":
        return prebuilt_tools(root)
    if choice == "build":
        return existing_tools(root) or built_tools(root)
    tools = existing_tools(root)
    if tools:
        return tools
    try:
        return prebuilt_tools(root)
    except SystemExit:
        raise
    except Exception as e:
        print(f"the prebuilt download failed ({e}); falling back to a local build")
        return built_tools(root)


def ensure_cli(tools: Tools) -> "pathlib.Path | None":
    """llama-cli for the smoke test. Free in the prebuilt tarball; a second
    build target otherwise."""
    if tools.cli:
        return pathlib.Path(tools.cli)
    if tools.kind == "build":
        return built_tools(tools.root, targets=("llama-cli",)).cli
    return None


def usable_gguf(path: pathlib.Path) -> bool:
    """Reject a leftover truncated GGUF instead of quantizing it.

    The magic alone is not enough: it is written before the tensors, so a
    crashed conversion leaves a file that passes a magic check and fails
    hours later. f16 of this model is ~1.4 GB (751.6M tensor elements, the
    tied embedding materialized), so anything under 500 MiB is a stump.
    """
    with path.open("rb") as fh:
        if fh.read(4) != b"GGUF":
            print(f"{path} is not a GGUF file; reconverting")
            return False
    if path.stat().st_size < 500 << 20:
        print(f"{path} is only {mib(path)}, far short of the ~1.4 GB expected; "
              f"treating it as truncated and reconverting")
        return False
    return True


def sha256(path: pathlib.Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def mib(path: pathlib.Path) -> str:
    return f"{path.stat().st_size / (1 << 20):.0f} MiB"


def smoke(tools: Tools, gguf: pathlib.Path) -> None:
    """Feed the quantized model one real validation prompt and print what it says.

    The prompt comes out of `val.jsonl`, so it is byte-identical to what
    `normalize::render_chat_prompt` builds at runtime -- pre-closed <think>
    block and all -- without this script needing the Rust source. llama-cli
    tokenizes `-p` with parse_special=true, which is what makes the
    <|im_start|> markers land as single tokens rather than literal text.
    """
    val = HERE / "val.jsonl"
    if not val.is_file():
        print(f"skipping smoke test: {val} is missing")
        return
    cli = ensure_cli(tools)
    if cli is None:
        print("skipping smoke test: no llama-cli available")
        return
    row = json.loads(val.read_text(encoding="utf-8").splitlines()[0])
    print("\n--- prompt ---")
    print(row["prompt"])
    print(f"--- expected completion ---\n{row['completion']}\n--- what the GGUF says ---")
    try:
        run([cli, "-m", gguf, "--no-cnv", "-no-display-prompt",
             "-p", row["prompt"], "-n", "256", "--temp", "0", "-c", "2048"],
            env=tools.env)
    except subprocess.CalledProcessError as e:
        print(f"\nsmoke test failed to run ({e}); the GGUF itself may still be fine")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--model-dir", type=pathlib.Path, default=HERE / "s1-mini-de",
                    help="the directory train_s1_de.py saved (default: %(default)s)")
    ap.add_argument("--llama-cpp", type=pathlib.Path, default=HERE / "llama.cpp",
                    help="llama.cpp checkout, cloned here if absent (default: %(default)s)")
    ap.add_argument("--quantizer", default="auto", choices=["auto", "prebuilt", "build"],
                    help="how to obtain llama-quantize; prebuilt needs no compiler "
                         "and no root (default: %(default)s)")
    ap.add_argument("--outtype", default="f16", choices=["f16", "bf16", "f32"],
                    help="intermediate precision; bf16 is lossless from bf16 weights "
                         "but f16 is the better-trodden path (default: %(default)s)")
    ap.add_argument("--quant", default="Q4_K_M",
                    help="final quant, e.g. Q4_K_M Q5_K_M Q6_K (default: %(default)s)")
    ap.add_argument("--keep-intermediate", action="store_true",
                    help="keep the ~1.4 GB unquantized GGUF")
    ap.add_argument("--smoke", action="store_true",
                    help="also run one validation prompt through the result")
    args = ap.parse_args()

    model_dir = args.model_dir.resolve()
    if not (model_dir / "config.json").is_file():
        sys.exit(f"{model_dir} has no config.json -- did train_s1_de.py finish and save?")
    for required in ("tokenizer.json", "tokenizer_config.json"):
        if not (model_dir / required).is_file():
            sys.exit(f"{model_dir}/{required} is missing; the converter needs the tokenizer "
                     f"(train_s1_de.py's `tok.save_pretrained` writes it)")

    root = ensure_checkout(args.llama_cpp.resolve())
    intermediate = HERE / f"s1-mini-de-{args.outtype}.gguf"
    final = HERE / f"s1-mini-de-{args.quant.lower()}.gguf"

    if intermediate.is_file() and usable_gguf(intermediate):
        print(f"reusing {intermediate} ({mib(intermediate)})")
    else:
        ensure_convert_deps()
        # Convert to a .part name and rename only on success, so a conversion
        # that dies partway (a missing vocab dependency, an OOM) cannot leave a
        # truncated GGUF for the next run's reuse branch to pick up.
        partial = intermediate.with_name(intermediate.name + ".part")
        partial.unlink(missing_ok=True)
        # sys.executable, not "python": the converter must import the torch and
        # transformers from the environment that produced these weights.
        run([sys.executable, root / "convert_hf_to_gguf.py", model_dir,
             "--outfile", partial, "--outtype", args.outtype])
        partial.replace(intermediate)

    tools = get_tools(root, args.quantizer)
    run([tools.quantize, intermediate, final, args.quant, str(os.cpu_count() or 4)],
        env=tools.env)

    if not args.keep_intermediate:
        intermediate.unlink()
        print(f"removed {intermediate}")

    if args.smoke:
        smoke(tools, final)

    digest = sha256(final)
    print(f"""
--- done ---
  {final}
  {mib(final)}   sha256 {digest}

Install it on the machine that runs yappr. Keep the upstream file first -- it
is your only rollback:

  cd {YAPPR_MODELS_DIR}
  mv {YAPPR_MODEL_FILE} s1-mini-q4_k_m-untuned.gguf
  scp <trainhost>:{final} ./{YAPPR_MODEL_FILE}

The name is fixed: `llama.rs`'s MODEL_FILE is `{YAPPR_MODEL_FILE}` whatever
quant is actually inside.

Then pin the new hash BY HAND, in the runtime lock only:

  {YAPPR_MODELS_DIR}/models.lock.toml
      s1-mini = "{digest}"

DO NOT run `yappr --update-lock` to do this. It calls `models::download_all`,
which sees a file whose sha256 disagrees with the pin, re-downloads upstream
S1-mini, finds *that* matches the pin, and promotes it over your finetune --
destroying it and leaving the lock unchanged. Editing the runtime lock instead
makes `download_all` take its `continue // already good` branch and touch
nothing. Leave `crates/yappr-core/models.lock.toml` alone: it is compiled into
the binary and must keep matching what the HuggingFace URL serves, for a fresh
install's integrity check.

With the hash pinned, verify against the real engine:

  cargo test -p yappr-core --lib llama:: -- --ignored
""")


if __name__ == "__main__":
    main()
