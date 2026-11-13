//! Process-wide monotonic clock shared by all capture sources.

use std::sync::OnceLock;
use std::time::Instant;

/// Microseconds since a process-wide epoch (first call).
pub fn mono_us() -> u64 {
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_micros() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic() {
        let a = mono_us();
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(mono_us() > a);
    }
}
