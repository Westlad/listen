#!/usr/bin/env python3
"""openWakeWord model sidecar for openclaw-listen.

This process does not open the microphone. Rust owns the CPAL input stream and
writes 16-bit 16 kHz mono PCM to stdin. The sidecar only runs the wake model and
prints a JSON wake event to stdout.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

import numpy as np
from openwakeword.model import Model


SAMPLE_RATE_HZ = 16_000
CHUNK_MS = 80


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--model-path", required=True)
    parser.add_argument("--threshold", type=float, default=0.5)
    parser.add_argument("--chunk-ms", type=int, default=CHUNK_MS)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    model_path = Path(args.model_path).expanduser()
    if not model_path.exists():
        print(f"wake model not found: {model_path}", file=sys.stderr, flush=True)
        return 2

    model = Model(wakeword_models=[str(model_path)])
    frame_samples = max(1, int(SAMPLE_RATE_HZ * args.chunk_ms / 1000))
    frame_bytes = frame_samples * 2

    print(
        json.dumps(
            {
                "event": "ready",
                "model_path": str(model_path),
                "threshold": args.threshold,
                "sample_rate_hz": SAMPLE_RATE_HZ,
                "chunk_ms": args.chunk_ms,
            }
        ),
        file=sys.stderr,
        flush=True,
    )

    stdin = sys.stdin.buffer
    while True:
        chunk = stdin.read(frame_bytes)
        if not chunk:
            return 0
        if len(chunk) < frame_bytes:
            continue

        frame = np.frombuffer(chunk, dtype=np.int16)
        prediction = model.predict(frame)
        for name, score in prediction.items():
            score = float(score)
            if score >= args.threshold:
                print(
                    json.dumps(
                        {
                            "event": "wake",
                            "model": name,
                            "score": score,
                        }
                    ),
                    flush=True,
                )
                return 0


if __name__ == "__main__":
    raise SystemExit(main())
