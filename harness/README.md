# store-io performance harness

Measures store-io against the raw platform primitive and against fsys 1.1.3 on the machine it runs on. A standalone workspace, never published: it depends on fsys only to compare against it.

```text
cd harness
cargo run --release -- all            # every workload; a few minutes
cargo run --release -- lone           # one workload: lone, concurrent, caller-batch, page-batch, sequential
cargo run --release -- --help
```

Each run writes `results/<date>-<os>-<device>.{md,json}`: the device report and evidence, the exact store options, every point with throughput and latency percentiles, a summary table against raw and fsys, and any anomaly or integrity failure. Every workload verifies its data at least once. On a device store-io refuses (a virtual disk, a `nobarrier` mount) the run uses the labelled override and says so at the top of the report.

A consumer NVMe drive is noisy: the raw primitive itself can swing 30% or more between identical runs, so differences under about 15% in a single run are not findings. Repeat or A/B a point before acting on it.
