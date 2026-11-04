#!/usr/bin/env python3
"""Plain OpenTofu on the host: the floor under every orchestrator's run time."""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))
import harness as h  # noqa: E402

if __name__ == "__main__":
    os.makedirs(h.WORK, exist_ok=True)
    h.save("baseline", {"tool": "tofu (no orchestrator)", "version": h.TOFU_VERSION,
                        "runs": h.baseline_tofu(runs=int(os.environ.get("SERIAL_RUNS", 5)))})
