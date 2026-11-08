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

## V4 mix and parallelism decision (Task 10)

- Machine: Apple M1 (arm64), macOS. Date: 2026-11-08.
- Setup: the baseline's 400 `MockVenue` pools plus 50 `testkit::v4_fixture_quoting` pools (native ETH against a distinct token each, fee 3000, spacing 60, one position over ticks -600..600, liquidity 10_000_000_000, two bitmap words each side), each paired with a `V2Pool` whose quote reserve is raised by `1% + i * 0.02%`. The bench asserts that all 50 V4 pairs find a trade before timing.
- Variants: sequential `find_all`, and a temporary `find_all_par` that evaluated the pair groups with rayon `par_iter`.

| Case | `release` | `release-max` (fat LTO, 1 codegen unit) |
| --- | --- | --- |
| `find_all_all_pairs`, sequential | 20.292 ms (an earlier noisy run gave 23.764 ms) | 19.593 ms |
| `find_all_all_pairs`, rayon | 4.3290 ms (earlier run 7.8271 ms) | 5.9251 ms |
| `find_all_one_touched`, sequential | 94.149 µs (earlier run 120.46 µs) | 88.770 µs |

Medians from criterion. The sequential and rayon rows of each column come from the same run.

Decisions:

- Rayon: kept. Its median beats sequential by 79% in `release` (4.33 ms against 20.29 ms) and by 70% in `release-max`, well past the 20% bar. `find_all` now evaluates groups with `par_iter`, the temporary variant is gone, and `rayon = "1"` stays in `Cargo.toml`.
- Profile: `make run` stays on `release`. `release-max` beats `release` by 3.4% on the sequential median (19.59 ms against 20.29 ms), under the 10% bar.

After the change (`release`, parallel `find_all`): `find_all_all_pairs` 4.8675 ms, `find_all_one_touched` 183.52 µs. Rayon's fan out costs about 90 µs when one group is touched, which is the common case per block; a sequential path for small touched sets would win that back if it matters.
