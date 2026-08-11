#!/usr/bin/env python3
"""Vance image generator — a thin Hugging Face `diffusers` wrapper.

Vance installs this into its managed venv (pip: torch diffusers transformers
accelerate safetensors) and runs it to turn a prompt into a PNG. The model is
pulled from Hugging Face on first use and cached; `--prewarm` fetches/validates
the model at install time so the first real generation isn't a multi-GB wait.

Progress lines are printed with flush=True and model-download bars go to stderr,
so Vance's install/gen progress stays live. Exit code is non-zero on failure.
"""
import argparse
import sys


def log(msg: str) -> None:
    print(f"[vance-imagegen] {msg}", flush=True)


def pick_device():
    import torch

    if torch.cuda.is_available():
        return "cuda", torch.float16
    mps = getattr(torch.backends, "mps", None)
    if mps is not None and mps.is_available():
        # float32 on MPS avoids the black-image issue some SD builds hit in fp16.
        return "mps", torch.float32
    return "cpu", torch.float32


def load_pipe(model: str, device: str, dtype):
    from diffusers import AutoPipelineForText2Image

    log(f"loading model '{model}' on {device}")
    pipe = AutoPipelineForText2Image.from_pretrained(model, torch_dtype=dtype)
    return pipe.to(device)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--model", required=True, help="HF model id or local path")
    ap.add_argument("--prompt")
    ap.add_argument("--negative", default=None)
    ap.add_argument("--width", type=int, default=512)
    ap.add_argument("--height", type=int, default=512)
    ap.add_argument("--steps", type=int, default=25)
    ap.add_argument("--seed", type=int, default=None)
    ap.add_argument("--out")
    ap.add_argument("--prewarm", action="store_true",
                    help="fetch + validate the model, then exit (no image)")
    args = ap.parse_args()

    device, dtype = pick_device()

    if args.prewarm:
        # Download + validate on CPU (no accelerator memory needed just to cache).
        load_pipe(args.model, "cpu", dtype)
        log("model ready")
        return 0

    if not args.prompt or not args.out:
        log("error: --prompt and --out are required")
        return 2

    import torch

    pipe = load_pipe(args.model, device, dtype)
    try:
        pipe.set_progress_bar_config(disable=False)
    except Exception:
        pass

    # A CPU generator seeds deterministically and works on every backend
    # (MPS can't host a torch.Generator).
    generator = None
    if args.seed is not None and args.seed >= 0:
        generator = torch.Generator(device="cpu").manual_seed(args.seed)

    is_turbo = "turbo" in args.model.lower()
    steps = 1 if (is_turbo and args.steps <= 0) else max(1, args.steps)

    kwargs = dict(
        prompt=args.prompt,
        width=args.width,
        height=args.height,
        num_inference_steps=steps,
    )
    if args.negative:
        kwargs["negative_prompt"] = args.negative
    if is_turbo:
        kwargs["guidance_scale"] = 0.0
    if generator is not None:
        kwargs["generator"] = generator

    log("generating")
    image = pipe(**kwargs).images[0]
    image.save(args.out)
    log(f"wrote {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
