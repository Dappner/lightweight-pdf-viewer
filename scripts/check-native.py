#!/usr/bin/env python3
"""Check a native window benchmark and optionally compare its UI delays to a baseline."""

import argparse
import json
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("report", type=Path)
parser.add_argument("--before", type=Path)
parser.add_argument("--max-ui-ms", type=float, default=500.0)
args = parser.parse_args()
report = json.loads(args.report.read_text())
before = json.loads(args.before.read_text()) if args.before else None
reference = {p["phase"]: p for p in before["phases"]} if before else {}
failures = []
print(f"Metal buffers per window: {report['metal_buffers_per_window']}")
for phase in report["phases"]:
    name = phase["phase"]
    delay = phase["ui_intervals"]
    previous = reference.get(name)
    comparison = (
        f" (before: p95 {previous['ui_intervals']['p95_ms']:.1f}, "
        f"max {previous['ui_intervals']['max_ms']:.1f} ms)" if previous else ""
    )
    print(f"{name}: p95 {delay['p95_ms']:.1f}, max {delay['max_ms']:.1f} ms{comparison}")
    if delay["count"] < 3:
        failures.append(f"{name}: too few frames to measure")
    if delay["max_ms"] > args.max_ui_ms:
        failures.append(f"{name}: UI delay exceeds {args.max_ui_ms:g} ms")
    if name in ("paired_resize", "zoom_first", "zoom_second") and phase["actions"] < 3:
        failures.append(f"{name}: too few actions to exercise the workload")
    if name == "paired_resize" and phase["distinct_sizes"] < 6:
        failures.append("paired_resize: native windows did not report enough size changes")
if failures:
    raise SystemExit("\n".join(failures))
print("PASS: workloads completed without UI stalls above the threshold")
