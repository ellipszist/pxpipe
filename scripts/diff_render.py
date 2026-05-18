#!/usr/bin/env python3
"""
Diff harness: compare Python render_chunks vs Rust render_chunks pixel-by-pixel.

Strategy:
  1. For each sample text, call Python render_chunks() -> list[PNG bytes]
  2. Call Rust diff_render binary -> writes PNGs to disk
  3. Decode both sides' PNGs and compare pixel arrays
  4. Report: pass / fail with first diff coordinates

Run:
  python3 scripts/diff_render.py
"""

import hashlib
import json
import subprocess
import sys
import tempfile
from pathlib import Path

# Make src/proxy.py importable
ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "src"))

import proxy as py_proxy  # noqa: E402
from PIL import Image  # noqa: E402
import io  # noqa: E402

RUST_BIN = ROOT / "src" / "rust" / "target" / "release" / "examples" / "diff_render"

SAMPLES = {
    "short_ascii": "hello world\n",
    "multiline_code": (
        "def fibonacci(n):\n"
        "    if n <= 1: return n\n"
        "    return fibonacci(n-1) + fibonacci(n-2)\n"
        "print(fibonacci(10))\n"
    ),
    "long_with_blanks": (
        "# Header\n\n"
        + "\n".join(f"line {i}: " + "x" * (i % 60) for i in range(200))
        + "\n\n# Footer\n"
    ),
    "many_short_lines": "\n".join(f"L{i}" for i in range(500)) + "\n",
    "wide_lines": "\n".join("." * 80 for _ in range(100)) + "\n",
    "tools_like_json": json.dumps(
        {"tools": [{"name": f"tool_{i}", "input_schema": {"type": "object",
         "properties": {f"p{j}": {"type": "string"} for j in range(5)}}}
         for i in range(20)]}, indent=2),
}


def png_to_pixels(png_bytes: bytes):
    """Decode PNG to (width, height, raw_grayscale_bytes)."""
    img = Image.open(io.BytesIO(png_bytes))
    img = img.convert("L")  # force 8-bit grayscale
    return img.width, img.height, img.tobytes()


def run_rust(samples: dict, out_dir: Path) -> dict:
    """Invoke rust diff_render binary, returns name -> list[png_bytes]."""
    spec = {name: text for name, text in samples.items()}
    spec_path = out_dir / "spec.json"
    spec_path.write_text(json.dumps(spec))
    res = subprocess.run(
        [str(RUST_BIN), str(spec_path), str(out_dir)],
        capture_output=True, text=True,
    )
    if res.returncode != 0:
        print("RUST FAILED:", res.stderr, file=sys.stderr)
        sys.exit(2)
    manifest = json.loads((out_dir / "manifest.json").read_text())
    out = {}
    for name, files in manifest.items():
        out[name] = [(out_dir / f).read_bytes() for f in files]
    return out


def diff_pixels(py_bytes: bytes, rs_bytes: bytes):
    """Return None if pixel-identical, else (reason, details)."""
    pw, ph, pp = png_to_pixels(py_bytes)
    rw, rh, rp = png_to_pixels(rs_bytes)
    if (pw, ph) != (rw, rh):
        return ("dim_mismatch", f"py={pw}x{ph} rs={rw}x{rh}")
    if pp == rp:
        return None
    # Find first diff
    for i, (a, b) in enumerate(zip(pp, rp)):
        if a != b:
            x = i % pw
            y = i // pw
            return ("pixel_mismatch", f"first diff at ({x},{y}): py={a} rs={b}")
    return ("byte_len_mismatch", f"py_len={len(pp)} rs_len={len(rp)}")


def main():
    if not RUST_BIN.exists():
        print(f"build rust first: cd src/rust && cargo build --release --example diff_render")
        print(f"missing: {RUST_BIN}")
        sys.exit(1)

    with tempfile.TemporaryDirectory() as td:
        out_dir = Path(td)
        rust_out = run_rust(SAMPLES, out_dir)

        total = 0
        fail = 0
        for name, text in SAMPLES.items():
            py_pngs = py_proxy.render_chunks(text)
            rs_pngs = rust_out.get(name, [])
            total += 1
            print(f"\n[{name}]  py_chunks={len(py_pngs)} rs_chunks={len(rs_pngs)}")
            if len(py_pngs) != len(rs_pngs):
                fail += 1
                print(f"  FAIL: chunk count differs")
                continue
            any_fail = False
            for i, (a, b) in enumerate(zip(py_pngs, rs_pngs)):
                ah = hashlib.sha256(a).hexdigest()[:12]
                bh = hashlib.sha256(b).hexdigest()[:12]
                diff = diff_pixels(a, b)
                if diff is None:
                    print(f"  chunk {i}: OK   py={ah} rs={bh} bytes={len(a)}/{len(b)}")
                else:
                    any_fail = True
                    print(f"  chunk {i}: FAIL {diff[0]}: {diff[1]}  py={ah} rs={bh}")
            if any_fail:
                fail += 1

        print(f"\n=== {total - fail}/{total} samples pixel-identical ===")
        sys.exit(0 if fail == 0 else 1)


if __name__ == "__main__":
    main()
