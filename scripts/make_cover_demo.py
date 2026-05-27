#!/usr/bin/env python3
"""Generate a 5M-row parquet for the v0.14 dev.to cover image.

Schema is deliberately simple (id BIGINT, country VARCHAR, revenue DOUBLE,
ts BIGINT) and `id` is monotonically increasing. That's important for the
demo: row groups inherit min/max stats from the underlying data, so a
filter like `where .id < 1000000` can be pruned by DuckDB at the row-group
level — about 80% of row groups get skipped, which is what produces the
green `● pruned: 80%` line in the cover screenshot.

Usage:
    python scripts/make_cover_demo.py /tmp/cover_demo.parquet
"""
from __future__ import annotations

import sys
from pathlib import Path

try:
    import numpy as np
    import pandas as pd
except ImportError:
    sys.exit("pandas + numpy required: pip install pandas pyarrow numpy")


def main() -> None:
    out = Path(sys.argv[1] if len(sys.argv) > 1 else "/tmp/cover_demo.parquet")
    n = 5_000_000

    rng = np.random.default_rng(seed=42)
    countries = np.array(["US", "UK", "DE", "FR", "JP", "CN"])

    df = pd.DataFrame({
        # Monotonic id — crucial: row groups end up with sequential
        # min/max so DuckDB can skip them on `where .id < N` filters.
        "id": np.arange(1, n + 1, dtype=np.int64),
        "country": countries[rng.integers(0, 6, size=n)],
        "revenue": np.round(rng.random(n) * 1000.0, 2),
        "ts": (rng.random(n) * 86_400).astype(np.int64),
    })

    # Default row-group size for pyarrow is 64K rows; that gives ~76
    # row groups for 5M, which is enough variation to make pruning
    # demonstrable but not so many the metadata read dominates.
    df.to_parquet(out, index=False)
    print(f"wrote {out} ({n:,} rows)")


if __name__ == "__main__":
    main()
