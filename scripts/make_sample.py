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


def make_users(out: Path) -> None:
    rows = [
        {"id": 1, "email": "alice@example.com",   "country": "US", "revenue": 1245.00, "age": 32},
        {"id": 2, "email": "bob@example.com",     "country": "CA", "revenue": 89.50,   "age": 27},
        {"id": 3, "email": "claire@example.org",  "country": "US", "revenue": 17820.00,"age": 41},
        {"id": 4, "email": "dieter@example.de",   "country": "DE", "revenue": 312.00,  "age": 55},
        {"id": 5, "email": "eve@example.co.uk",   "country": "UK", "revenue": 45.00,   "age": 19},
        {"id": 6, "email": "frank@example.com",   "country": "US", "revenue": 0.00,    "age": 8},
        {"id": 7, "email": "grace@example.fr",    "country": "FR", "revenue": 999.99,  "age": 67},
    ]
    pd.DataFrame(rows).to_parquet(out, index=False)
    print(f"wrote {out} ({len(rows)} rows)")


def make_orders(out: Path) -> None:
    """Companion file for join demos — `user_id` is the FK to sample.parquet.id."""
    rows = [
        {"order_id": 101, "user_id": 1, "amount":  45.00, "status": "shipped"},
        {"order_id": 102, "user_id": 1, "amount":  12.50, "status": "shipped"},
        {"order_id": 103, "user_id": 3, "amount": 999.00, "status": "shipped"},
        {"order_id": 104, "user_id": 3, "amount":  20.00, "status": "refunded"},
        {"order_id": 105, "user_id": 4, "amount": 312.00, "status": "shipped"},
        {"order_id": 106, "user_id": 7, "amount":   8.99, "status": "pending"},
        {"order_id": 107, "user_id": 7, "amount":  15.00, "status": "shipped"},
    ]
    pd.DataFrame(rows).to_parquet(out, index=False)
    print(f"wrote {out} ({len(rows)} rows)")


def make_hive_demo(root: Path) -> None:
    """Tiny hive-partitioned dataset for the --hive demo."""
    import shutil
    if root.exists():
        shutil.rmtree(root)
    data = [
        ("dt=2026-05-19", "region=US", [
            {"order_id": 1, "amount": 100.0},
            {"order_id": 2, "amount": 250.0},
        ]),
        ("dt=2026-05-19", "region=EU", [
            {"order_id": 3, "amount":  50.0},
        ]),
        ("dt=2026-05-20", "region=US", [
            {"order_id": 4, "amount": 312.0},
            {"order_id": 5, "amount":  17.0},
            {"order_id": 6, "amount": 999.0},
        ]),
        ("dt=2026-05-20", "region=EU", [
            {"order_id": 7, "amount":  45.0},
        ]),
    ]
    for dt_seg, region_seg, rows in data:
        d = root / dt_seg / region_seg
        d.mkdir(parents=True, exist_ok=True)
        pd.DataFrame(rows).to_parquet(d / "part-00000.parquet", index=False)
    n = sum(len(rows) for _, _, rows in data)
    print(f"wrote {root}/ ({n} rows across {len(data)} partitions)")


def main() -> None:
    out = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("sample.parquet")
    make_users(out)
    # Companion files for demos. Names are fixed because demo.sh references them.
    make_orders(out.parent / "orders.parquet")
    make_hive_demo(out.parent / "sales")


if __name__ == "__main__":
    main()
