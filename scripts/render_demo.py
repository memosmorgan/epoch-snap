#!/usr/bin/env python3
"""Run readme_demo.rs against the local library and render its stdout as a GIF.

Regenerate from the repository root: python3 scripts/render_demo.py
Requires Rust/Cargo, Python 3, Pillow, and a monospaced TrueType font.
By default fontconfig selects Noto Sans Mono; --font accepts a font file instead.
No font or rendering library is bundled or added to the Rust project.

Frames reveal actual command output with fixed reading pauses, not execution
timings. The demo's assertions and exit status must pass before any GIF is saved.
The displayed ./readme-demo command is the executable actually run in a temporary
directory. Build output stays outside the recording. No terminal output is mocked.
"""

import argparse
import json
from pathlib import Path
import subprocess
import tempfile

from PIL import Image, ImageDraw, ImageFont


def capture_demo(root):
    subprocess.run(["cargo", "build", "--locked", "--lib"], cwd=root, check=True)
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
        cwd=root, text=True,
    ))
    target = Path(metadata["target_directory"]) / "debug"
    with tempfile.TemporaryDirectory(prefix="epochsnap-readme-demo-") as temporary:
        executable = Path(temporary) / "readme-demo"
        subprocess.run([
            "rustc", "--edition=2024", "-D", "warnings",
            str(root / "scripts/readme_demo.rs"),
            "--extern", f"epochsnap={target / 'libepochsnap.rlib'}",
            "-L", f"dependency={target / 'deps'}", "-o", str(executable),
        ], cwd=root, check=True)
        result = subprocess.run(
            ["./readme-demo"], cwd=temporary, text=True,
            capture_output=True, check=True,
        )
    if result.stderr:
        raise RuntimeError(result.stderr)
    return result.stdout


def render(stdout, font_path, output):
    font = ImageFont.truetype(str(font_path), 18)
    title_font = ImageFont.truetype(str(font_path), 14)
    if font.getlength("i") != font.getlength("W"):
        raise ValueError("choose a monospaced font")
    lines = ["$ ./readme-demo", *stdout.splitlines()]
    if len(lines) != 8:
        raise ValueError("unexpected demo output; review the frame schedule")
    if any(font.getlength(line) > 772 for line in lines):
        raise ValueError("demo output exceeds terminal width")

    # Header output is shown together; each subsequent state gets its own pause.
    visible_counts = [1, 3, 4, 5, 6, 7, 8]
    durations = [900, 1100, 1300, 1300, 1300, 1300, 2600]
    frames = []
    for count in visible_counts:
        frame = Image.new("RGB", (820, 320), "#0d1117")
        draw = ImageDraw.Draw(frame)
        draw.rectangle((0, 0, 819, 319), outline="#48515b")
        draw.line((1, 38, 818, 38), fill="#30363d")
        draw.text((24, 11), "EpochSnap / Stopped capture", font=title_font, fill="#9da7b1")
        for index, line in enumerate(lines[:count]):
            color = "#e6edf3"
            if index == 0 or line.startswith("verified"):
                color = "#7ee787"
            elif index in (1, 2):
                color = "#9da7b1"
            draw.text((24, 60 + 28 * index), line, font=font, fill=color)
        frames.append(frame)

    # One palette avoids color changes between frames; no dithering is needed
    # for a flat terminal. Small frame deltas keep the looping GIF compact.
    palette = frames[-1].quantize(colors=32, dither=Image.Dither.NONE)
    indexed = [frame.quantize(palette=palette, dither=Image.Dither.NONE) for frame in frames]
    output.parent.mkdir(parents=True, exist_ok=True)
    indexed[0].save(
        output, save_all=True, append_images=indexed[1:], duration=durations,
        loop=0, disposal=2, optimize=False,
    )
    print(stdout, end="")
    print(f"Saved {output.relative_to(Path(__file__).resolve().parent.parent)} "
          f"({output.stat().st_size:,} bytes; {sum(durations) / 1000:.1f}s; 820x320)")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--font", type=Path, help="monospaced TrueType font file")
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    font_path = args.font
    if font_path is None:
        font_path = Path(subprocess.check_output(
            ["fc-match", "-f", "%{file}", "Noto Sans Mono"], text=True,
        ))
    stdout = capture_demo(root)
    render(stdout, font_path, root / "docs/assets/rewind-demo.gif")


if __name__ == "__main__":
    main()
