//! Optional network impairment for testing: latency, jitter, loss and link flaps.
use anyhow::{Context, Result, bail};
use rand_core::{OsRng, RngCore};
use std::time::Duration;

/// Applied by a session to everything it sends. Datagrams are delayed, jittered and
/// dropped; the reliable control stream is delayed and stalled across flaps but never
/// loses data (as a retransmitting transport would behave).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Impairment {
    pub latency: Duration,
    pub jitter: Duration,
    /// Probability in 0..=1 that a datagram is dropped.
    pub loss: f32,
    /// `(down, every)`: the link is down for the last `down` of every `every` period.
    pub flap: Option<(Duration, Duration)>,
}

fn parse_duration(s: &str) -> Result<Duration> {
    let (num, unit): (&str, fn(u64) -> Duration) = if let Some(n) = s.strip_suffix("ms") {
        (n, Duration::from_millis)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, Duration::from_secs)
    } else {
        bail!("duration {s:?} needs a ms or s suffix");
    };
    Ok(unit(
        num.parse().with_context(|| format!("bad duration {s:?}"))?,
    ))
}

impl Impairment {
    /// Parses `latency=20ms,jitter=5ms,loss=0.02,flap=2s/300s`; every key is optional.
    pub fn parse(s: &str) -> Result<Impairment> {
        let mut i = Impairment::default();
        for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (k, v) = part
                .split_once('=')
                .with_context(|| format!("expected key=value, got {part:?}"))?;
            match k {
                "latency" => i.latency = parse_duration(v)?,
                "jitter" => i.jitter = parse_duration(v)?,
                "loss" => {
                    i.loss = v.parse().with_context(|| format!("bad loss {v:?}"))?;
                    if !(0.0..=1.0).contains(&i.loss) {
                        bail!("loss must be within 0..=1");
                    }
                }
                "flap" => {
                    let (d, e) = v.split_once('/').context("flap must be down/every")?;
                    let (d, e) = (parse_duration(d)?, parse_duration(e)?);
                    if e.is_zero() || d > e {
                        bail!("flap needs 0 < down <= every");
                    }
                    i.flap = Some((d, e));
                }
                _ => bail!("unknown impairment key {k:?}"),
            }
        }
        Ok(i)
    }

    /// Reads `NOVALINK_IMPAIR`. A malformed value is logged and ignored.
    pub fn from_env() -> Option<Impairment> {
        let raw = std::env::var("NOVALINK_IMPAIR").ok()?;
        match Impairment::parse(&raw) {
            Ok(i) => Some(i),
            Err(e) => {
                tracing::warn!("ignoring NOVALINK_IMPAIR: {e:#}");
                None
            }
        }
    }

    /// Whether the link is down `elapsed` after impairment started.
    pub fn in_flap(&self, elapsed: Duration) -> bool {
        self.flap.is_some_and(|(down, every)| {
            let phase = elapsed.as_nanos() % every.as_nanos();
            phase >= every.as_nanos() - down.as_nanos()
        })
    }

    /// Time until the current flap ends (zero when the link is up).
    pub fn flap_remaining(&self, elapsed: Duration) -> Duration {
        match self.flap {
            Some((_, every)) if self.in_flap(elapsed) => {
                let phase = elapsed.as_nanos() % every.as_nanos();
                Duration::from_nanos((every.as_nanos() - phase) as u64)
            }
            _ => Duration::ZERO,
        }
    }

    pub(crate) fn roll_loss(&self) -> bool {
        self.loss > 0.0 && (unit_random() as f32) < self.loss
    }

    pub(crate) fn delay(&self) -> Duration {
        self.latency + self.jitter.mul_f64(unit_random())
    }
}

/// Uniform in `[0, 1)`.
fn unit_random() -> f64 {
    (OsRng.next_u64() >> 11) as f64 / (1u64 << 53) as f64
}
