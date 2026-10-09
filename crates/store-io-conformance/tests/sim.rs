//! The conformance suite on the deterministic simulator.

#![allow(clippy::unwrap_used)]

use std::path::Path;

use store_io_conformance::Suite;
use store_io_core::decide::Trust;
use store_io_platform::Platform;
use store_io_sim::{SimConfig, SimPlatform};

#[test]
fn test_the_simulator_conforms() {
    for cfg in [SimConfig::volatile(1), SimConfig::power_safe(2)] {
        let p = SimPlatform::new(cfg);
        let _root = p.open_dir(Path::new("/conformance"), true).unwrap();
        let report = Suite::new(&p, "/conformance", Trust::default()).run();
        assert!(report.passed(), "\n{report}");
        assert_eq!(report.outcomes.len(), Suite::<SimPlatform>::names().len());
    }
}
