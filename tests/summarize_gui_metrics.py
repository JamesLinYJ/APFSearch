#!/usr/bin/env python3
"""Summarize opt-in production GUI timings recorded after real input events.

The measured boundary ends after AppKit displayIfNeeded submission. It includes
input debounce, authenticated XPC, JSON and view updates, not the compositor's
presentation of the frame. This script never generates UI events itself.
"""
import argparse
import collections
import datetime
import hashlib
import json
import math
import pathlib
import plistlib


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("metrics", type=pathlib.Path)
    parser.add_argument("baseline", type=pathlib.Path)
    parser.add_argument("--list-id", required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--application", type=pathlib.Path, default=pathlib.Path("/Applications/APFSearch.app"))
    parser.add_argument("--warmups", type=int, default=2)
    parser.add_argument("--minimum-samples", type=int, default=30)
    args = parser.parse_args()
    baseline = json.loads(args.baseline.read_text())
    observations = collections.defaultdict(list)
    for line in args.metrics.read_text().splitlines():
        row = json.loads(line)
        if (row.get("list_id") == args.list_id and row.get("input_event")
                and row.get("visible") and "end_to_end_ms" in row):
            observations[(row["query"], json.dumps(row.get("sort"), sort_keys=True))].append(row)
    results = []
    for specification in baseline["queries"]:
        if specification["sort"] != [{"ascending": True, "field": "name"}]:
            continue  # Column-order interaction is validated separately.
        rows = observations[(specification["query"], json.dumps(specification["sort"], sort_keys=True))][args.warmups:]
        samples = [row["end_to_end_ms"] for row in rows]
        ordered = sorted(samples)
        p95 = ordered[math.ceil(len(ordered) * .95) - 1] if ordered else None
        results.append({
            "name": specification["name"], "query": specification["query"],
            "sort": specification["sort"],
            "sample_count": len(samples), "samples_ms": samples,
            "p95_ms": p95, "target_passed": p95 is not None and p95 <= 100,
            "counts_correct": bool(rows) and all(
                row["total"] == specification["total_matches"] for row in rows),
            "enough_samples": len(rows) >= args.minimum_samples,
        })
    info = plistlib.loads((args.application / "Contents/Info.plist").read_bytes())
    executable = args.application / "Contents/MacOS" / info["CFBundleExecutable"]
    report = {
        "timestamp": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "dataset": "1,000,000 synthetic offline records",
        "list_id": args.list_id,
        "application": str(args.application),
        "application_sha256": hashlib.sha256(executable.read_bytes()).hexdigest(),
        "build": info["CFBundleVersion"],
        "boundary": "Real input event through AppKit displayIfNeeded submission; compositor frame presentation excluded",
        "warmups_excluded_per_query": args.warmups,
        "queries": results,
        "complete": all(row["enough_samples"] for row in results),
        "success": all(row["counts_correct"] and row["target_passed"]
                  and row["enough_samples"] for row in results),
    }
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({"success": report["success"], "complete": report["complete"],
                      "samples": [row["sample_count"] for row in results]}))
    return 0 if report["success"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
