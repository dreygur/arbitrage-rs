# Step benchmark baseline

Constant product baseline for `find_all` (no decision taken from it here).

- Machine: Apple M1 (arm64), macOS
- Profile: `cargo bench --features testkit` (release, opt-level 3, no LTO)
- Date: 2026-11-08
- Setup: 200 pairs on two venues (400 `MockVenue` pools), fee 30 bps, base reserve 1_000_000_000, quote reserve 1_000_000_000 on the first venue and raised by `i * 0.05%` on the second venue, `max_trade` 100_000_000.

| Case | Median |
| --- | --- |
| `find_all_all_pairs` (`touched = None`) | 3.9043 ms |
| `find_all_one_touched` (one pool touched) | 87.503 µs |
