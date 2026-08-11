#!/usr/bin/env python3
"""Vance image generator — a thin Hugging Face `diffusers` wrapper.

Vance installs this into its managed venv (pip: torch diffusers transformers
accelerate safetensors) and runs it to turn a prompt into a PNG. The model is
pulled from Hugging Face on first use and cached; `--prewarm` fetches/validates
the model at install time so the first real generation isn't a multi-GB wait.

Design notes:
- Resource-constrained so it doesn't peg/freeze the machine: capped CPU threads,
  attention + VAE slicing to keep peak memory down, fp16 on GPU.
- `--watch-parent`: exits the moment Vance (which holds our stdin) goes away —
  even on force-quit — so a heavy generation never keeps running orphaned.
- Prints `step N/M` each diffusion step (and flush'd status lines) so Vance can
  show real progress instead of appearing frozen.
"""
import argparse
import os
import sys
import threading


def log(msg: str) -> None:
    print(f"[vance-imagegen] {msg}", flush=True)


def start_parent_watchdog() -> None:
    """Exit if the parent process (Vance) disappears. Vance holds our stdin
    pipe; when it dies the read returns EOF immediately (works even for SIGKILL),
    so a machine-pegging generation can't outlive the app."""
    def watch():
        try:
            while sys.stdin.buffer.read(1):
                pass
        except Exception:
            pass
        os._exit(1)

    threading.Thread(target=watch, daemon=True).start()


def constrain_cpu() -> None:
    """Leave headroom so the UI/OS stay responsive during CPU inference."""
    try:
        import multiprocessing
        threads = max(1, (multiprocessing.cpu_count() or 4) // 2)
    except Exception:
        threads = 2
    os.environ.setdefault("OMP_NUM_THREADS", str(threads))
    os.environ.setdefault("MKL_NUM_THREADS", str(threads))
    try:
        import torch
        torch.set_num_threads(threads)
    except Exception:
        pass


def pick_device():
    import torch

    if torch.cuda.is_available():
        return "cuda", torch.float16
    mps = getattr(torch.backends, "mps", None)
    if mps is not None and mps.is_available():
        # fp16 on MPS halves memory + is faster; SD-1.5 renders fine in fp16.
        return "mps", torch.float16
    return "cpu", torch.float32


def load_pipe(model: str, device: str, dtype):
    from diffusers import AutoPipelineForText2Image

    log(f"loading model '{model}' on {device}")
    pipe = AutoPipelineForText2Image.from_pretrained(model, torch_dtype=dtype)
    pipe = pipe.to(device)
    # Keep the memory peak low so we don't push the machine into swap.
    for fn in ("enable_attention_slicing", "enable_vae_slicing"):
        try:
            getattr(pipe, fn)()
        except Exception:
            pass
    return pipe


def clamp_dim(v: int) -> int:
    # SD wants multiples of 8; cap the size so a huge request can't OOM/freeze.
    v = max(256, min(1024, int(v)))
    return v - (v % 8)


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
    ap.add_argument("--watch-parent", action="store_true",
                    help="exit when the parent (stdin) closes")
    args = ap.parse_args()

    if args.watch_parent:
        start_parent_watchdog()
    constrain_cpu()

    device, dtype = pick_device()

    if args.prewarm:
        load_pipe(args.model, "cpu", dtype)
        log("model ready")
        return 0

    if not args.prompt or not args.out:
        log("error: --prompt and --out are required")
        return 2

    import torch

    pipe = load_pipe(args.model, device, dtype)

    generator = None
    if args.seed is not None and args.seed >= 0:
        generator = torch.Generator(device="cpu").manual_seed(args.seed)

    is_turbo = "turbo" in args.model.lower()
    steps = 1 if (is_turbo and args.steps <= 0) else max(1, args.steps)
    width, height = clamp_dim(args.width), clamp_dim(args.height)

    # Emit `step N/M` each diffusion step so Vance can show progress. Uses the
    # modern diffusers callback; falls back to no callback on older versions.
    def on_step(_pipe, step_index, _timestep, callback_kwargs):
        log(f"step {step_index + 1}/{steps}")
        return callback_kwargs

    kwargs = dict(
        prompt=args.prompt,
        width=width,
        height=height,
        num_inference_steps=steps,
    )
    if args.negative:
        kwargs["negative_prompt"] = args.negative
    if is_turbo:
        kwargs["guidance_scale"] = 0.0
    if generator is not None:
        kwargs["generator"] = generator

    log(f"generating {width}x{height}, {steps} steps on {device}")
    try:
        image = pipe(callback_on_step_end=on_step, **kwargs).images[0]
    except TypeError:
        # Older diffusers without callback_on_step_end.
        image = pipe(**kwargs).images[0]
    image.save(args.out)
    log(f"wrote {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
