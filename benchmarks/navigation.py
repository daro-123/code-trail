"""Measure full-response latency and top-result correctness on synthetic Java projects."""
import argparse
import json
from pathlib import Path
import statistics
import subprocess
import tempfile
import time


def percentile(values, fraction):
    return sorted(values)[min(len(values) - 1, int((len(values) - 1) * fraction))]


def measure(binary, files, repeats):
    with tempfile.TemporaryDirectory(prefix="code-trail-bench-") as directory:
        root = Path(directory)
        (root / "pom.xml").write_text("<project/>\n")
        source = root / "Use.java"
        source.write_text("import sample0.Target;\nclass Use { Target target; }\n")
        expected = root / "sample0" / "Target.java"
        for index in range(files):
            folder = root / f"sample{index}"
            folder.mkdir()
            (folder / "Target.java").write_text(f"package sample{index};\npublic class Target {{ public void execute() {{}} }}\n")
        started = time.perf_counter_ns()
        with subprocess.Popen([str(binary)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True) as process:
            durations = []
            correct = 0
            for index in range(repeats + 1):
                request = {"version": 1, "id": index, "method": "definition", "params": {"file": str(source), "line": 2, "column": 12, "symbol": "Target"}}
                begin = started if index == 0 else time.perf_counter_ns()
                process.stdin.write(json.dumps(request) + "\n")
                process.stdin.flush()
                response = json.loads(process.stdout.readline())
                durations.append((time.perf_counter_ns() - begin) / 1_000_000)
                if "error" in response:
                    raise RuntimeError(response["error"])
                results = response["result"]
                correct += bool(results and Path(results[0]["file"]) == expected)
            process.stdin.close()
            process.wait(timeout=10)
            if process.returncode:
                raise RuntimeError(f"server exited with {process.returncode}")
        return {"java_files": files + 1, "requests": repeats + 1, "cold_ms": round(durations[0], 3), "warm_median_ms": round(statistics.median(durations[1:]), 3), "warm_p95_ms": round(percentile(durations[1:], .95), 3), "top_result_correct": correct, "correct_rate": correct / (repeats + 1)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/code-trail"))
    parser.add_argument("--sizes", type=int, nargs="+", default=[10, 100, 1000])
    parser.add_argument("--repeats", type=int, default=20)
    args = parser.parse_args()
    if args.repeats < 1 or any(size < 1 for size in args.sizes):
        parser.error("sizes and repeats must be positive")
    print(json.dumps({"benchmark": "synthetic Java ambiguous type lookup", "cold_includes_process_start": True, "filesystem_cache": "uncontrolled", "results": [measure(args.binary.resolve(), size, args.repeats) for size in args.sizes]}, indent=2))


if __name__ == "__main__":
    main()
