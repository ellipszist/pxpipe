#!/usr/bin/env python3
"""
Render the same text through Python and Rust, save PNGs side-by-side, and
also stitch a single comparison image with labels for easy visual inspection.

Usage:
  python3 scripts/visual_compare.py [out_dir]

Output:
  <out_dir>/python_<sample>.png
  <out_dir>/rust_<sample>.png
  <out_dir>/compare_<sample>.png    (side-by-side with labels)
  <out_dir>/README.txt              (dim + byte size summary)
"""

import io
import json
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "src"))

import proxy as py_proxy  # noqa: E402
from PIL import Image, ImageDraw, ImageFont  # noqa: E402

RUST_BIN = ROOT / "src" / "rust" / "target" / "release" / "examples" / "diff_render"

# Three representative samples
SAMPLES = {
    "tiny": "echo hello world\n",
    "code": (
        "def render_chunks(text):\n"
        "    \"\"\"Render text into 5pt PNG, multi-column.\"\"\"\n"
        "    font = get_font(FONT_SIZE)\n"
        "    lines = wrap(text, FIXED_COL_CHARS)\n"
        "    return paint(font, lines)\n"
    ),
    "tools_schema": json.dumps({
        "tools": [
            {
                "name": "Bash",
                "description": "Execute a bash command",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "command": {"type": "string", "description": "the command"},
                        "timeout": {"type": "integer", "description": "timeout in ms"},
                    },
                    "required": ["command"],
                },
            },
            {
                "name": "Read",
                "description": "Read a file from disk",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "file_path": {"type": "string"},
                        "offset": {"type": "integer"},
                        "limit": {"type": "integer"},
                    },
                    "required": ["file_path"],
                },
            },
        ]
    }, indent=2),
}


def label_image(img: Image.Image, label: str) -> Image.Image:
    """Add a black bar at top with the label in white."""
    bar_h = 24
    out = Image.new("RGB", (img.width, img.height + bar_h), (0, 0, 0))
    out.paste(img.convert("RGB"), (0, bar_h))
    d = ImageDraw.Draw(out)
    try:
        f = ImageFont.truetype("/System/Library/Fonts/Menlo.ttc", 14)
    except Exception:
        f = ImageFont.load_default()
    d.text((6, 4), label, fill=(255, 255, 255), font=f)
    return out


def main():
    out_dir = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("/tmp/cip_compare")
    out_dir.mkdir(parents=True, exist_ok=True)

    if not RUST_BIN.exists():
        print(f"build rust first: cd src/rust && cargo build --release --example diff_render")
        sys.exit(1)

    # Run rust
    spec_path = out_dir / "spec.json"
    spec_path.write_text(json.dumps(SAMPLES))
    rust_outdir = out_dir / "_rust"
    rust_outdir.mkdir(exist_ok=True)
    r = subprocess.run(
        [str(RUST_BIN), str(spec_path), str(rust_outdir)],
        capture_output=True, text=True,
    )
    if r.returncode != 0:
        print("RUST FAILED:", r.stderr); sys.exit(2)
    rust_manifest = json.loads((rust_outdir / "manifest.json").read_text())

    summary_lines = ["Visual compare: Python proxy.py vs Rust src/rust/", ""]

    for name, text in SAMPLES.items():
        # Python
        py_pngs = py_proxy.render_chunks(text)
        assert len(py_pngs) == 1, f"unexpected multi-chunk python output for {name}"
        py_path = out_dir / f"python_{name}.png"
        py_path.write_bytes(py_pngs[0])
        py_img = Image.open(io.BytesIO(py_pngs[0]))

        # Rust
        rs_files = rust_manifest.get(name, [])
        assert len(rs_files) == 1, f"unexpected multi-chunk rust output for {name}"
        rs_bytes = (rust_outdir / rs_files[0]).read_bytes()
        rs_path = out_dir / f"rust_{name}.png"
        rs_path.write_bytes(rs_bytes)
        rs_img = Image.open(io.BytesIO(rs_bytes))

        # Side-by-side compare
        py_labeled = label_image(py_img, f"PYTHON {py_img.width}x{py_img.height} ({len(py_pngs[0])}B)")
        rs_labeled = label_image(rs_img, f"RUST   {rs_img.width}x{rs_img.height} ({len(rs_bytes)}B)")
        gap = 8
        cmp_w = max(py_labeled.width, rs_labeled.width)
        cmp_h = py_labeled.height + gap + rs_labeled.height
        cmp = Image.new("RGB", (cmp_w, cmp_h), (40, 40, 40))
        cmp.paste(py_labeled, (0, 0))
        cmp.paste(rs_labeled, (0, py_labeled.height + gap))
        cmp.save(out_dir / f"compare_{name}.png", "PNG")

        line = (
            f"[{name:14s}] py={py_img.width:4d}x{py_img.height:<4d} ({len(py_pngs[0]):>6d}B)   "
            f"rs={rs_img.width:4d}x{rs_img.height:<4d} ({len(rs_bytes):>6d}B)   "
            f"text_len={len(text)}"
        )
        print(line)
        summary_lines.append(line)

    (out_dir / "README.txt").write_text("\n".join(summary_lines) + "\n")
    print(f"\nwrote PNGs to {out_dir}/")


if __name__ == "__main__":
    main()
