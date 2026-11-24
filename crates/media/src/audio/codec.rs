//! Opus voice codec (48 kHz mono, 20 ms frames, in-band FEC).

use opus::{Application, Bitrate, Channels, Decoder, Encoder};

use super::{FRAME_SAMPLES, SAMPLE_RATE};
use crate::{MediaError, MediaResult};

/// Max Opus packet we produce; well under any QUIC datagram limit.
pub const MAX_PACKET: usize = 1000;
pub const VOICE_BITRATE: i32 = 48_000;

fn err(e: opus::Error) -> MediaError {
    MediaError::Codec(e.to_string())
}

pub struct VoiceEncoder {
    enc: Encoder,
    buf: Vec<u8>,
}

impl VoiceEncoder {
    pub fn new() -> MediaResult<Self> {
        let mut enc = Encoder::new(SAMPLE_RATE, Channels::Mono, Application::Voip).map_err(err)?;
        enc.set_bitrate(Bitrate::Bits(VOICE_BITRATE)).map_err(err)?;
        enc.set_inband_fec(true).map_err(err)?;
        enc.set_packet_loss_perc(10).map_err(err)?;
        Ok(Self {
            enc,
            buf: vec![0; MAX_PACKET],
        })
    }

    /// Encodes one 20 ms frame of [`FRAME_SAMPLES`] samples.
    pub fn encode(&mut self, pcm: &[f32]) -> MediaResult<&[u8]> {
        debug_assert_eq!(pcm.len(), FRAME_SAMPLES);
        let n = self.enc.encode_float(pcm, &mut self.buf).map_err(err)?;
        Ok(&self.buf[..n])
    }
}

pub struct VoiceDecoder {
    dec: Decoder,
}

impl VoiceDecoder {
    pub fn new() -> MediaResult<Self> {
        Ok(Self {
            dec: Decoder::new(SAMPLE_RATE, Channels::Mono).map_err(err)?,
        })
    }

    /// Decodes a packet into `out` ([`FRAME_SAMPLES`] long).
    pub fn decode(&mut self, packet: &[u8], out: &mut [f32]) -> MediaResult<()> {
        let n = self.dec.decode_float(packet, out, false).map_err(err)?;
        out[n..].fill(0.0);
        Ok(())
    }

    /// Recovers the *previous* (lost) frame from the FEC data in `next_packet`.
    pub fn decode_fec(&mut self, next_packet: &[u8], out: &mut [f32]) -> MediaResult<()> {
        let n = self.dec.decode_float(next_packet, out, true).map_err(err)?;
        out[n..].fill(0.0);
        Ok(())
    }

    /// Packet loss concealment for one frame.
    pub fn conceal(&mut self, out: &mut [f32]) -> MediaResult<()> {
        let n = self.dec.decode_float(&[], out, false).map_err(err)?;
        out[n..].fill(0.0);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_roundtrip_snr() {
        let mut enc = VoiceEncoder::new().expect("enc");
        let mut dec = VoiceDecoder::new().expect("dec");
        let mut signal = Vec::new();
        let mut decoded = Vec::new();
        for f in 0..50 {
            let frame: Vec<f32> = (0..FRAME_SAMPLES)
                .map(|i| {
                    let t = (f * FRAME_SAMPLES + i) as f32 / SAMPLE_RATE as f32;
                    (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5
                })
                .collect();
            let pkt = enc.encode(&frame).expect("encode").to_vec();
            assert!(pkt.len() < MAX_PACKET);
            let mut out = vec![0.0; FRAME_SAMPLES];
            dec.decode(&pkt, &mut out).expect("decode");
            signal.extend(frame);
            decoded.extend(out);
        }
        // Opus has algorithmic delay; find the best alignment within 20 ms.
        let skip = 10 * FRAME_SAMPLES;
        let best = (0..FRAME_SAMPLES)
            .map(|lag| {
                let (mut s, mut n) = (0.0f64, 0.0f64);
                for i in skip..signal.len() - FRAME_SAMPLES {
                    let a = f64::from(signal[i]);
                    let b = f64::from(decoded[i + lag]);
                    s += a * a;
                    n += (a - b) * (a - b);
                }
                10.0 * (s / n.max(1e-12)).log10()
            })
            .fold(f64::MIN, f64::max);
        assert!(best > 10.0, "SNR {best:.1} dB");

        let mut plc = vec![0.0; FRAME_SAMPLES];
        dec.conceal(&mut plc).expect("plc");
    }
}
