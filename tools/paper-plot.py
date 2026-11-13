#!/usr/bin/env python3
"""Render a PEGLINE_PAPER_TRACE CSV as a self-contained HTML/SVG report.

The trace is written by the real scene, not by a reduced simulation. Run
`pwsh tools/paper-trace.ps1` first, then:

    python tools/paper-plot.py target/paper-trace.csv -o target/paper-trace.html

The report plots the dragged card's angle, target, angular velocity and pin
velocity, and prints the numbers that matter for tuning: peak angle, zero
crossings, settling time and steady tilt. No third-party packages.
"""

from __future__ import annotations

import argparse
import csv
import html
import math
import sys
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path


@dataclass
class Row:
    t: float
    id: int
    grabbed: bool
    hop: bool
    release: bool
    x: float
    vx: float
    drop: float
    vy: float
    angle: float
    omega: float
    target: float
    ax: float
    ay: float
    g_eff: float
    wind: float
    flex: float
    swap_dir: int


def number(row: Mapping[str, str | None], key: str, default: float = 0.0) -> float:
    text = row.get(key)
    if text is None:
        return default
    try:
        value = float(text)
    except ValueError:
        return default
    return value if math.isfinite(value) else default


def flag(row: Mapping[str, str | None], key: str) -> bool:
    return (row.get(key) or "").strip() in {"1", "true", "True"}


def integer(row: Mapping[str, str | None], key: str, default: int = 0) -> int:
    try:
        return int(number(row, key, float(default)))
    except (ValueError, OverflowError):
        return default


def load(path: Path) -> list[Row]:
    with path.open("r", encoding="utf-8", newline="") as handle:
        reader = csv.DictReader(handle)
        return [
            Row(
                t=number(raw, "t"),
                id=integer(raw, "id"),
                grabbed=flag(raw, "grabbed"),
                hop=flag(raw, "hop"),
                release=flag(raw, "release"),
                x=number(raw, "x"),
                vx=number(raw, "vx"),
                drop=number(raw, "drop"),
                vy=number(raw, "vy"),
                angle=number(raw, "angle"),
                omega=number(raw, "omega"),
                target=number(raw, "target"),
                ax=number(raw, "ax"),
                ay=number(raw, "ay"),
                g_eff=number(raw, "g_eff"),
                wind=number(raw, "wind"),
                flex=number(raw, "flex"),
                swap_dir=integer(raw, "swap_dir"),
            )
            for raw in reader
        ]


def pick_dragged(rows: list[Row]) -> int:
    grabbed = [row for row in rows if row.grabbed]
    if grabbed:
        return grabbed[0].id
    by_id: dict[int, float] = {}
    for row in rows:
        by_id[row.id] = max(by_id.get(row.id, 0.0), abs(row.omega))
    if not by_id:
        return 0
    return max(by_id, key=lambda key: by_id[key])


def panel(
    title: str,
    unit: str,
    series: list[tuple[str, str, list[tuple[float, float]], bool]],
    tmax: float,
    release: float,
    show_time_axis: bool,
) -> str:
    width, height = 1000, 170
    left, right, top = 76, 24, 28
    bottom = 30 if show_time_axis else 8
    plot_w = width - left - right
    plot_h = height - top - bottom
    values = [value for _, _, points, _ in series for _, value in points]
    lo, hi = min(values + [0.0]), max(values + [0.0])
    if hi - lo < 1e-6:
        lo, hi = lo - 1.0, hi + 1.0
    pad = (hi - lo) * 0.12
    lo, hi = lo - pad, hi + pad

    def sx(t: float) -> float:
        return left + (t / tmax if tmax > 0 else 0.0) * plot_w

    def sy(v: float) -> float:
        return top + (hi - v) / (hi - lo) * plot_h

    out = [
        f'<svg viewBox="0 0 {width} {height}" role="img" aria-label="{html.escape(title)}">',
        f'<text x="{left}" y="18" font-size="14" font-weight="600" fill="#1b1b1b">{html.escape(title)}'
        f'<tspan font-weight="400" fill="#666"> ({html.escape(unit)})</tspan></text>',
    ]
    for i in range(5):
        v = lo + (hi - lo) * i / 4.0
        y = sy(v)
        out.append(
            f'<line x1="{left}" y1="{y:.1f}" x2="{width-right}" y2="{y:.1f}" stroke="#e8e8e8"/>'
        )
        out.append(
            f'<text x="{left-8}" y="{y+4:.1f}" text-anchor="end" font-size="10" fill="#777">{v:.2f}</text>'
        )
    if lo < 0.0 < hi:
        out.append(
            f'<line x1="{left}" y1="{sy(0):.1f}" x2="{width-right}" y2="{sy(0):.1f}" stroke="#c9c9c9"/>'
        )
    if show_time_axis:
        for i in range(6):
            t = tmax * i / 5.0
            x = sx(t)
            out.append(
                f'<text x="{x:.1f}" y="{height-8}" text-anchor="middle" font-size="10" fill="#777">{t:.2f}s</text>'
            )
    release_x = sx(release)
    if 0.0 <= release <= tmax:
        out.append(
            f'<line x1="{release_x:.1f}" y1="{top}" x2="{release_x:.1f}" y2="{top+plot_h}" '
            f'stroke="#d04a4a" stroke-dasharray="4 3"/>'
        )
        out.append(
            f'<text x="{release_x+4:.1f}" y="{top+11}" font-size="10" fill="#d04a4a">release</text>'
        )

    legend_x = width - right
    for label, color, points, dashed in reversed(series):
        dash = ' stroke-dasharray="5 3"' if dashed else ""
        out.append(
            f'<polyline fill="none" stroke="{color}" stroke-width="1.6"{dash} '
            f'points="{" ".join(f"{sx(t):.1f},{sy(v):.1f}" for t, v in points)}"/>'
        )
        out.append(
            f'<text x="{legend_x}" y="18" text-anchor="end" font-size="11" fill="{color}">{html.escape(label)}</text>'
        )
        legend_x -= max(70, 8 * len(label))
    out.append("</svg>")
    return "\n".join(out)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("csv", type=Path)
    parser.add_argument("-o", "--output", type=Path, default=None)
    args = parser.parse_args()
    rows = load(args.csv)
    if not rows:
        print("empty trace", file=sys.stderr)
        return 2
    dragged = pick_dragged(rows)
    card = sorted((row for row in rows if row.id == dragged), key=lambda row: row.t)
    if not card:
        print(f"no rows for card {dragged}", file=sys.stderr)
        return 2
    t0 = min(row.t for row in card)
    for row in card:
        row.t -= t0
    tmax = max(row.t for row in card)
    grabbed_rows = [row for row in card if row.grabbed]
    release = max((row.t for row in grabbed_rows), default=0.0)
    after = [row for row in card if row.t >= release]
    peak = max((abs(row.angle) for row in after), default=0.0)
    max_omega = max((abs(row.omega) for row in card), default=0.0)
    max_vx = max((abs(row.vx) for row in card), default=0.0)
    max_vy = max((abs(row.vy) for row in card), default=0.0)
    final_angle = card[-1].angle
    crossings = 0
    previous = 0.0
    for row in after:
        angle = row.angle
        if abs(angle) < 1e-4:
            continue
        if previous != 0.0 and (angle > 0.0) != (previous > 0.0):
            crossings += 1
        previous = angle
    last_moving = release
    for row in after:
        if abs(row.angle) > 0.1 or abs(row.omega) > 0.5:
            last_moving = row.t
    settle = max(0.0, last_moving - release)

    series_angle = [
        ("angle", "#0a5fbf", [(row.t, row.angle) for row in card], False),
        ("target", "#9a9a9a", [(row.t, row.target) for row in card], True),
    ]
    series_omega = [("omega", "#7a3db8", [(row.t, row.omega) for row in card], False)]
    series_v = [
        ("vx", "#1b8a5a", [(row.t, row.vx) for row in card], False),
        ("vy", "#d07a1f", [(row.t, row.vy) for row in card], False),
    ]
    chart = "\n".join(
        [
            panel("Pendulum angle", "deg", series_angle, tmax, release, False),
            panel("Angular velocity", "deg/s", series_omega, tmax, release, False),
            panel("Pin velocity", "DIP/s", series_v, tmax, release, True),
        ]
    )
    output = args.output or args.csv.with_suffix(".html")
    metrics = [
        ("card id", str(dragged)),
        ("trace span", f"{tmax:.2f} s"),
        ("release", f"{release:.2f} s"),
        ("peak |angle| after release", f"{peak:.2f} deg"),
        ("zero crossings after release", str(crossings)),
        ("settle (|angle|>0.1° or |ω|>0.5°/s)", f"{settle * 1000:.0f} ms"),
        ("max |omega|", f"{max_omega:.1f} deg/s"),
        ("max |vx| / |vy|", f"{max_vx:.0f} / {max_vy:.0f} DIP/s"),
        ("final angle", f"{final_angle:.2f} deg"),
    ]
    table = "\n".join(
        f"<tr><th>{html.escape(key)}</th><td>{html.escape(value)}</td></tr>"
        for key, value in metrics
    )
    document = f"""<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<title>Pegline paper trace — card {dragged}</title>
<style>
body {{ font: 13px/1.5 system-ui, sans-serif; margin: 24px; color: #1b1b1b; background: #fff; }}
h1 {{ font-size: 20px; margin: 0 0 4px; }}
p {{ color: #666; margin: 0 0 16px; }}
svg {{ width: 100%; max-width: 1040px; display: block; margin: 0 0 8px; }}
table {{ border-collapse: collapse; margin: 8px 0 20px; }}
th, td {{ border-bottom: 1px solid #eee; padding: 4px 16px 4px 0; text-align: left; }}
th {{ color: #666; font-weight: 500; }}
code {{ background: #f5f5f5; padding: 1px 4px; }}
</style></head><body>
<h1>Pegline paper trace — card {dragged}</h1>
<p>Real scene, <code>{html.escape(str(args.csv))}</code>. Dashed vertical line = pin release.</p>
{table}
{chart}
<p>Trace columns: t, id, grabbed, hop, release, x, vx, drop, vy, angle, omega, target, ax, ay, g_eff, wind, flex, swap_dir.</p>
</body></html>
"""
    output.write_text(document, encoding="utf-8")
    print(f"report: {output}")
    for key, value in metrics:
        print(f"  {key}: {value}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
