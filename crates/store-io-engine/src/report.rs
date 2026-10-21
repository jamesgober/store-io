//! How a [`StoreReport`] prints.

use crate::store::StoreReport;

impl core::fmt::Display for StoreReport {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let d = &self.decision;
        writeln!(f, "volume           {}", self.volume)?;
        writeln!(
            f,
            "io               {} ({})",
            self.io.interface, self.io.reason
        )?;
        writeln!(f, "class            {}", d.class)?;
        writeln!(f, "label            {}", d.label)?;
        writeln!(f, "durable open     {:?}", d.durable_open)?;
        if d.power_safe_candidate {
            writeln!(
                f,
                "note             device reports no volatile cache; health unreadable (power-safe candidate)"
            )?;
        }
        for r in d.reasons.iter() {
            writeln!(f, "reason           {r}")?;
        }
        for m in d.missing.iter() {
            writeln!(f, "missing          {m}")?;
        }
        writeln!(f, "flush execution  inline (leader)")?;
        writeln!(f, "block size       {}", self.block_size)?;
        writeln!(f, "table capacity   {} regions", self.table_capacity)?;
        writeln!(f, "filesystem       {:?}", self.evidence.fs.kind)?;
        writeln!(f, "bus              {:?}", self.evidence.device.bus)?;
        if let Some(m) = &self.evidence.device.model {
            writeln!(f, "model            {m}")?;
        }
        for u in &self.unavailable {
            writeln!(
                f,
                "unavailable      region {:?} (id {}, {:?}): {}",
                u.name,
                u.id.get(),
                u.kind,
                u.reason
            )?;
        }
        writeln!(f, "format           {}", self.format)?;
        if self.format == store_io_format::slot::FORMAT_2 {
            write!(f, "{}", self.metadata)?;
        }
        write!(
            f,
            "mode             {}",
            if self.read_only {
                "read-only"
            } else {
                "read-write"
            }
        )
    }
}
