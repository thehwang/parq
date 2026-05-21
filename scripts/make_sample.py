#!/usr/bin/env python3
"""Generate a tiny sample.parquet for local testing.

Usage:
    python scripts/make_sample.py            # writes ./sample.parquet
    python scripts/make_sample.py /tmp/x.parquet
"""
from __future__ import annotations

import sys
from pathlib import Path

try:
    import pandas as pd
except ImportError:
    sys.exit("pandas is required: pip install pandas pyarrow")


def main() -> None:
    out = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("sample.parquet")
    rows = [
        {"id": 1, "email": "alice@example.com",   "country": "US", "revenue": 1245.00, "age": 32},
        {"id": 2, "email": "bob@example.com",     "country": "CA", "revenue": 89.50,   "age": 27},
        {"id": 3, "email": "claire@cadent.tv",    "country": "US", "revenue": 17820.00,"age": 41},
        {"id": 4, "email": "dieter@example.de",   "country": "DE", "revenue": 312.00,  "age": 55},
        {"id": 5, "email": "eve@example.co.uk",   "country": "UK", "revenue": 45.00,   "age": 19},
        {"id": 6, "email": "frank@example.com",   "country": "US", "revenue": 0.00,    "age": 8},
        {"id": 7, "email": "grace@example.fr",    "country": "FR", "revenue": 999.99,  "age": 67},
    ]
    df = pd.DataFrame(rows)
    df.to_parquet(out, index=False)
    print(f"wrote {out} ({len(rows)} rows)")


if __name__ == "__main__":
    main()
