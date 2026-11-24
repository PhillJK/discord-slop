//! Per-speaker jitter buffer for 20 ms voice packets.
//!
//! Packets are reordered by sequence number and released one per playout tick.
//! The target depth adapts to measured inter-arrival jitter (2–6 frames,
//! i.e. 40–120 ms). Gaps are concealed with Opus in-band FEC from the next
//! packet when available, otherwise packet loss concealment (PLC). After a few
//! empty ticks the speaker is considered silent (they stopped transmitting) and
//! the buffer re-primes on the next packet.

use std::{collections::BTreeMap, time::Instant};

use bytes::Bytes;

const MIN_DEPTH: usize = 2;
const MAX_DEPTH: usize = 6;
/// Hard cap; beyond this the oldest packets are dropped to catch up.
const OVERFLOW: usize = 15;
/// Empty ticks with nothing buffered before the speaker is treated as silent.
const SILENCE_TICKS: u32 = 3;
const FRAME_MS: f32 = 20.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Playout {
    /// Decode this packet normally.
    Decode(Bytes),
    /// The expected packet is lost but the next one is here: decode its FEC data.
    Fec(Bytes),
    /// Lost, no FEC available: run PLC.
    Conceal,
    /// Nothing to play (not started / speaker silent).
    Silence,
}

pub struct JitterBuffer {
    packets: BTreeMap<u64, Bytes>,
    /// Extended (unwrapped) seq of the highest packet seen.
    highest: Option<u64>,
    next: Option<u64>,
    playing: bool,
    empty_ticks: u32,
    /// RFC 3550-style smoothed jitter estimate in ms.
    jitter_ms: f32,
    /// (arrival, timestamp, seq) of the previous packet.
    last_arrival: Option<(Instant, u32, u16)>,
    pub stats: JitterStats,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct JitterStats {
    pub received: u64,
    pub late: u64,
    pub concealed: u64,
    pub fec_recovered: u64,
    pub overflow_dropped: u64,
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl JitterBuffer {
    pub fn new() -> Self {
        Self {
            packets: BTreeMap::new(),
            highest: None,
            next: None,
            playing: false,
            empty_ticks: 0,
            jitter_ms: 0.0,
            last_arrival: None,
            stats: JitterStats::default(),
        }
    }

    /// Target buffering depth in frames.
    pub fn target_depth(&self) -> usize {
        let extra = (self.jitter_ms * 2.0 / FRAME_MS).ceil() as usize;
        (MIN_DEPTH + extra).clamp(MIN_DEPTH, MAX_DEPTH)
    }

    fn unwrap_seq(&self, seq: u16) -> u64 {
        let Some(h) = self.highest else {
            return u64::from(seq) + (1 << 16); // headroom for slightly older packets
        };
        let base = h & !0xffff;
        let candidates = [
            base.wrapping_sub(1 << 16) | u64::from(seq),
            base | u64::from(seq),
            (base + (1 << 16)) | u64::from(seq),
        ];
        candidates
            .into_iter()
            .min_by_key(|c| c.abs_diff(h))
            .unwrap_or(base | u64::from(seq))
    }

    /// Adds a received packet. `timestamp` is the sender's 48 kHz sample clock.
    pub fn push(&mut self, seq: u16, timestamp: u32, payload: Bytes) {
        self.push_at(seq, timestamp, payload, Instant::now());
    }

    pub fn push_at(&mut self, seq: u16, timestamp: u32, payload: Bytes, now: Instant) {
        self.stats.received += 1;
        // Jitter estimate: difference between arrival spacing and send spacing,
        // only across consecutive packets (talk-spurt gaps and reordering would
        // otherwise inflate it).
        if let Some((t0, ts0, seq0)) = self.last_arrival
            && seq == seq0.wrapping_add(1)
        {
            let arrival_ms = now.duration_since(t0).as_secs_f32() * 1000.0;
            let send_ms = timestamp.wrapping_sub(ts0) as i32 as f32 / 48.0;
            let d = (arrival_ms - send_ms).abs();
            if d < 1000.0 {
                self.jitter_ms += (d - self.jitter_ms) / 16.0;
            }
        }
        self.last_arrival = Some((now, timestamp, seq));

        let ext = self.unwrap_seq(seq);
        if self.next.is_some_and(|n| ext < n) {
            self.stats.late += 1;
            return;
        }
        self.highest = Some(self.highest.map_or(ext, |h| h.max(ext)));
        self.packets.insert(ext, payload);
        while self.packets.len() > OVERFLOW {
            if let Some((k, _)) = self.packets.pop_first() {
                self.stats.overflow_dropped += 1;
                self.next = Some(k + 1);
            }
        }
    }

    /// Called once per 20 ms playout tick.
    pub fn pop(&mut self) -> Playout {
        if !self.playing {
            if self.packets.len() < self.target_depth() {
                return Playout::Silence;
            }
            self.playing = true;
            self.empty_ticks = 0;
            self.next = self.packets.keys().next().copied();
        }
        let Some(next) = self.next else {
            return Playout::Silence;
        };
        // Too much buffered (e.g. after a network stall): skip ahead.
        if self.packets.len() > self.target_depth() + 3
            && let Some((&first, _)) = self.packets.first_key_value()
            && first >= next
        {
            let skip_to = self
                .highest
                .unwrap_or(first)
                .saturating_sub(self.target_depth() as u64 - 1);
            let mut n = next;
            while n < skip_to {
                if self.packets.remove(&n).is_some() {
                    self.stats.overflow_dropped += 1;
                }
                n += 1;
            }
            self.next = Some(n);
            return self.pop();
        }

        if let Some(p) = self.packets.remove(&next) {
            self.next = Some(next + 1);
            self.empty_ticks = 0;
            return Playout::Decode(p);
        }
        if self.packets.is_empty() {
            self.empty_ticks += 1;
            if self.empty_ticks > SILENCE_TICKS {
                self.playing = false;
                self.next = None;
                return Playout::Silence;
            }
            self.next = Some(next + 1);
            self.stats.concealed += 1;
            return Playout::Conceal;
        }
        self.next = Some(next + 1);
        if let Some(p) = self.packets.get(&(next + 1)) {
            self.stats.fec_recovered += 1;
            return Playout::Fec(p.clone());
        }
        self.stats.concealed += 1;
        Playout::Conceal
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn b(i: u8) -> Bytes {
        Bytes::from(vec![i])
    }

    fn feed_in_time(jb: &mut JitterBuffer, seqs: &[u16]) {
        let t0 = Instant::now();
        for (i, s) in seqs.iter().enumerate() {
            jb.push_at(
                *s,
                u32::from(*s) * 960,
                b(*s as u8),
                t0 + Duration::from_millis(20 * i as u64),
            );
        }
    }

    #[test]
    fn primes_then_plays_in_order() {
        let mut jb = JitterBuffer::new();
        jb.push(1, 960, b(1));
        assert_eq!(jb.pop(), Playout::Silence, "waits for target depth");
        jb.push(0, 0, b(0)); // reordered
        assert_eq!(jb.pop(), Playout::Decode(b(0)));
        assert_eq!(jb.pop(), Playout::Decode(b(1)));
    }

    #[test]
    fn loss_uses_fec_then_plc() {
        let mut jb = JitterBuffer::new();
        feed_in_time(&mut jb, &[0, 1, 3, 6]);
        assert_eq!(jb.pop(), Playout::Decode(b(0)));
        assert_eq!(jb.pop(), Playout::Decode(b(1)));
        assert_eq!(jb.pop(), Playout::Fec(b(3)), "2 lost, 3 present → FEC");
        assert_eq!(jb.pop(), Playout::Decode(b(3)));
        assert_eq!(jb.pop(), Playout::Conceal, "4 lost, 5 lost too → PLC");
        assert_eq!(jb.pop(), Playout::Fec(b(6)));
        assert_eq!(jb.pop(), Playout::Decode(b(6)));
    }

    #[test]
    fn late_packets_are_dropped() {
        let mut jb = JitterBuffer::new();
        feed_in_time(&mut jb, &[10, 11, 12]);
        assert_eq!(jb.pop(), Playout::Decode(b(10)));
        assert_eq!(jb.pop(), Playout::Decode(b(11)));
        jb.push(9, 9 * 960, b(9));
        assert_eq!(jb.stats.late, 1);
        assert_eq!(jb.pop(), Playout::Decode(b(12)));
    }

    #[test]
    fn goes_silent_and_reprimes() {
        let mut jb = JitterBuffer::new();
        feed_in_time(&mut jb, &[0, 1]);
        assert!(matches!(jb.pop(), Playout::Decode(_)));
        assert!(matches!(jb.pop(), Playout::Decode(_)));
        for _ in 0..SILENCE_TICKS {
            assert_eq!(jb.pop(), Playout::Conceal);
        }
        assert_eq!(jb.pop(), Playout::Silence);
        // Talk spurt resumes much later.
        feed_in_time(&mut jb, &[50, 51]);
        assert_eq!(jb.pop(), Playout::Decode(b(50)));
    }

    #[test]
    fn handles_sequence_wraparound() {
        let mut jb = JitterBuffer::new();
        feed_in_time(&mut jb, &[65534, 65535, 0, 1]);
        let got: Vec<_> = (0..4).map(|_| jb.pop()).collect();
        assert_eq!(
            got,
            vec![
                Playout::Decode(b(254)),
                Playout::Decode(b(255)),
                Playout::Decode(b(0)),
                Playout::Decode(b(1))
            ]
        );
    }

    #[test]
    fn overflow_catches_up() {
        let mut jb = JitterBuffer::new();
        let seqs: Vec<u16> = (0..12).collect();
        feed_in_time(&mut jb, &seqs);
        // Way more than target buffered → skips ahead so latency stays low.
        let first = jb.pop();
        let Playout::Decode(p) = first else {
            panic!("expected decode")
        };
        assert!(p[0] >= 8, "should have skipped stale audio, got {}", p[0]);
    }
}
