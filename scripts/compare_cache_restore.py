#!/usr/bin/env python3
"""Alternate cache restore binaries, preserving validation and resource counters.

Use matching immutable corpora already checked by compare_shared_layout.py.
This isolates recovery; it does not replace query or update acceptance.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for side in ("baseline", "candidate"):
        parser.add_argument(f"--{side}-binary", type=Path, required=True)
        parser.add_argument(f"--{side}-cache", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--rounds", type=int, default=5)
    args = parser.parse_args()
    if args.rounds < 1:
        parser.error("rounds must be positive")
    args.output.mkdir(mode=0o700, parents=True, exist_ok=False)
    records = {"baseline": [], "candidate": []}
    binary_sha256 = {side: hashlib.sha256(getattr(args, side + "_binary").read_bytes()).hexdigest()
                     for side in records}
    for number in range(args.rounds):
        sides = ("baseline", "candidate") if number % 2 == 0 else ("candidate", "baseline")
        for side in sides:
            result = subprocess.run([
                str(getattr(args, side + "_binary").resolve()), "--ignored", "--exact",
                "compact_acceptance_tests::cache_restore_resource_profile", "--nocapture",
            ], env=os.environ | {"APFSEARCH_ACCEPTANCE_CACHE": str(getattr(args, side + "_cache").resolve())},
                text=True, capture_output=True, timeout=300)
            log = args.output / f"{number + 1}-{side}.log"
            log.write_text(result.stdout + result.stderr)
            if result.returncode:
                raise RuntimeError(f"Recovery failed; inspect {log}")
            record = next(json.loads(line) for line in result.stdout.splitlines() if line.startswith('{"'))
            records[side].append(record)
            print(json.dumps({"round": number + 1, "side": side, "restore_ms": record["restore_ms"],
                              "phases": record["phases"]}), flush=True)
    if len({r["entries"] for runs in records.values() for r in runs}) != 1:
        raise AssertionError("Entry counts differ; use previously fingerprinted matching corpora")
    summary = {"scope": "Recovery only; no query, filesystem event or UI acceptance", "rounds": args.rounds,
               "binary_sha256": binary_sha256,
               "records": records, "medians": {}}
    for side, runs in records.items():
        resources = {field: statistics.median(r["restored"][field] for r in runs)
                     for field in ("resident_bytes", "physical_footprint_bytes", "peak_physical_footprint_bytes")}
        resources.update({field: statistics.median(r["restored"][field] - r["before"][field] for r in runs)
                          for field in ("cpu_seconds", "pageins", "minor_faults", "major_faults",
                                        "disk_bytes_read", "disk_bytes_written", "logical_writes")})
        summary["medians"][side] = {"restore_ms": statistics.median(r["restore_ms"] for r in runs),
            "phases_ms": {name: statistics.median(dict(r["phases"])[name] for r in runs)
                          for name, _ in runs[0]["phases"]}, "resources": resources}
    (args.output / "comparison.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary["medians"]), flush=True)


if __name__ == "__main__":
    main()
