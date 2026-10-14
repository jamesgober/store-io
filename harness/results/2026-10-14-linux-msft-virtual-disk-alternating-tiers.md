# Alternating-tier checks, 2026-10-14

WSL2 (kernel 6.6.87) on a Hyper-V virtual disk (ext4), library at commit
e4f90ec (step 3c) with the 3e fallback counter, harness at 3e. Each run is a
separate harness invocation with `--tier`; the runs alternate io_uring,
sync, io_uring, sync, back to back. Every report's `io` line confirms its
tier (`2026-10-14-alternating/*.md`, with the JSON beside each). Cells:
store-io / raw. Raw does not depend on the tier, so it shows the disk's
drift between runs.

## lone and caller-batch (3 s and 1 s points)

| run | QD1 durable append (ops/s) | QD1 page overwrite | 1 x append(4 KiB) + sync (records/s) | 10 x append(512 B) + sync | 100 x append(64 B) + sync | batch of 100 x 4 KiB |
|---|---|---|---|---|---|---|
| 1 io_uring | 871 / 1553 | 417 / 1553 | 368 / 368 | 2751 / 2752 | 4533 / 6300 | 34776 / 25304 |
| 2 sync | 338 / 385 | 240 / 385 | 386 / 402 | 2742 / 2742 | 6266 / 6347 | 28806 / 22990 |
| 3 io_uring | 389 / 377 | 358 / 377 | 384 / 369 | 2457 / 2766 | 4944 / 5009 | 18975 / 26461 |
| 4 sync | 386 / 372 | 380 / 372 | 389 / 382 | 2568 / 2623 | 4087 / 6237 | 9813 / 24306 |

## sequential (MB/s) and concurrent durable writers (ops/s), 3 s points

| run | 1 MiB writes | 2 MiB writes | 8 MiB writes | 2 writers | 8 writers | 64 writers |
|---|---|---|---|---|---|---|
| 1 io_uring | 2628 / 898 | 3146 / 342 | 3625 / 371 | 435 / 471 | 1422 / 443 | 8693 / 733 |
| 2 sync | 2587 / 2907 | 2430 / 2345 | 2575 / 1390 | 407 / 471 | 1494 / 431 | 8932 / 782 |
| 3 io_uring | 1473 / 2675 | 3126 / 2596 | 3974 / 1691 | 444 / 409 | 1461 / 420 | 7878 / 693 |
| 4 sync | 2156 / 1644 | 1814 / 1445 | 1599 / 1329 | 428 / 502 | 828 / 171 | 8234 / 736 |

## One durable writer (W2 N=1) and W1, 2 s points, six more runs (`alt2-*`)

The single-writer point of W2 was slower on io_uring in two of the runs
above (155 and 164 ops/s against 218 and 391 on sync; 1-writer rows of
`alt-*-concurrent.md`), with 0 fallbacks. Re-run, alternating six times:

| run | W2 1 writer (ops/s / p50 µs) | raw 1 writer | W2 2 writers | W1 QD1 durable append (ops/s / p50 µs) |
|---|---|---|---|---|
| 1 io_uring | 372 / 2299 | 404 | 2041 | 367 / 2566 |
| 2 sync | 374 / 2520 | 395 | 410 | 384 / 2435 |
| 3 io_uring | 369 / 2555 | 355 | 426 | 406 / 2299 |
| 4 sync | 393 / 2412 | 420 | 414 | 377 / 2489 |
| 5 io_uring | 386 / 2460 | 406 | 395 | 361 / 2579 |
| 6 sync | 383 / 2420 | 400 | 400 | 394 / 2367 |

Level: the earlier gap does not reproduce.

Reading: the two tiers scatter alike in lone, caller-batch and concurrent (the one-writer point after its re-run);
in sequential writes io_uring is equal or ahead at 5 of 6 points (1 MiB in
run 3 is the exception, with raw also moving 898 to 2,907 between runs).
Two runs per tier on this disk cannot rule out a small effect; the NVMe
measurement before the release can.
