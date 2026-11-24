//! Audio device I/O via cpal.
//!
//! Devices are opened at 48 kHz when supported so no resampling is needed.
//! `cpal::Stream` is not `Send` on every platform, so streams must be created
//! and kept on one thread (see [`super::engine`]).

use cpal::{
    Sample, SampleFormat, StreamConfig, SupportedStreamConfig,
    traits::{DeviceTrait, HostTrait},
};

use super::SAMPLE_RATE;
use crate::{MediaError, MediaResult};

fn dev_err(e: impl std::fmt::Display) -> MediaError {
    MediaError::Device(e.to_string())
}

fn device_name(d: &cpal::Device) -> Option<String> {
    d.description().ok().map(|desc| desc.name().to_owned())
}

/// Unique, user-meaningful names (ALSA lists several PCMs per card with the
/// same description, plus a null device).
fn clean_names(names: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    names
        .filter(|n| !n.starts_with("Discard all samples"))
        .filter(|n| seen.insert(n.clone()))
        .collect()
}

pub fn input_device_names() -> Vec<String> {
    let host = cpal::default_host();
    host.input_devices()
        .map(|it| clean_names(it.filter_map(|d| device_name(&d))))
        .unwrap_or_default()
}

pub fn output_device_names() -> Vec<String> {
    let host = cpal::default_host();
    host.output_devices()
        .map(|it| clean_names(it.filter_map(|d| device_name(&d))))
        .unwrap_or_default()
}

fn find_input(name: Option<&str>) -> MediaResult<cpal::Device> {
    let host = cpal::default_host();
    if let Some(name) = name
        && let Ok(mut devs) = host.input_devices()
        && let Some(d) = devs.find(|d| device_name(d).as_deref() == Some(name))
    {
        return Ok(d);
    }
    host.default_input_device()
        .ok_or_else(|| MediaError::Device("no input device".into()))
}

fn find_output(name: Option<&str>) -> MediaResult<cpal::Device> {
    let host = cpal::default_host();
    if let Some(name) = name
        && let Ok(mut devs) = host.output_devices()
        && let Some(d) = devs.find(|d| device_name(d).as_deref() == Some(name))
    {
        return Ok(d);
    }
    host.default_output_device()
        .ok_or_else(|| MediaError::Device("no output device".into()))
}

/// Prefers a 48 kHz config (f32 if possible), falling back to the default.
fn pick_config(
    supported: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
    default: MediaResult<SupportedStreamConfig>,
) -> MediaResult<SupportedStreamConfig> {
    let mut best: Option<SupportedStreamConfig> = None;
    for range in supported {
        if let Some(c) = range.try_with_sample_rate(SAMPLE_RATE) {
            let better = match &best {
                None => true,
                Some(b) => {
                    (c.sample_format() == SampleFormat::F32
                        && b.sample_format() != SampleFormat::F32)
                        || (c.channels() < b.channels() && c.sample_format() == b.sample_format())
                }
            };
            if better {
                best = Some(c);
            }
        }
    }
    best.map_or(default, Ok)
}

/// A live input stream (keep it alive on the creating thread).
pub struct Input {
    pub stream: cpal::Stream,
    pub sample_rate: u32,
}

/// Opens the input device; `on_samples` receives mono f32 samples at
/// `Input::sample_rate`.
pub fn open_input(
    name: Option<&str>,
    mut on_samples: impl FnMut(&[f32]) + Send + 'static,
) -> MediaResult<Input> {
    let dev = find_input(name)?;
    let supported = pick_config(
        dev.supported_input_configs().map_err(dev_err)?,
        dev.default_input_config().map_err(dev_err),
    )?;
    let format = supported.sample_format();
    let config: StreamConfig = supported.config();
    let channels = usize::from(config.channels.max(1));
    let sample_rate = config.sample_rate;
    let mut mono = Vec::<f32>::new();

    let err_fn = |e| tracing::warn!(error = %e, "input stream error");
    macro_rules! input_stream {
        ($t:ty) => {{
            dev.build_input_stream::<$t, _, _>(
                config,
                move |data: &[$t], _| {
                    mono.clear();
                    mono.extend(data.chunks_exact(channels).map(|frame| {
                        frame.iter().map(|s| s.to_sample::<f32>()).sum::<f32>() / channels as f32
                    }));
                    on_samples(&mono);
                },
                err_fn,
                None,
            )
        }};
    }
    let stream = match format {
        SampleFormat::F32 => input_stream!(f32),
        SampleFormat::I16 => input_stream!(i16),
        SampleFormat::U16 => input_stream!(u16),
        SampleFormat::I32 => input_stream!(i32),
        other => {
            return Err(MediaError::Device(format!(
                "unsupported input format {other:?}"
            )));
        }
    }
    .map_err(dev_err)?;
    Ok(Input {
        stream,
        sample_rate,
    })
}

/// A live output stream (keep it alive on the creating thread).
pub struct Output {
    pub stream: cpal::Stream,
    pub sample_rate: u32,
    pub channels: usize,
}

/// Opens the output device; `fill` must write mono samples at
/// `Output::sample_rate` into the slice (they are duplicated to all channels).
pub fn open_output(
    name: Option<&str>,
    mut fill: impl FnMut(&mut [f32]) + Send + 'static,
) -> MediaResult<Output> {
    let dev = find_output(name)?;
    let supported = pick_config(
        dev.supported_output_configs().map_err(dev_err)?,
        dev.default_output_config().map_err(dev_err),
    )?;
    let format = supported.sample_format();
    let config: StreamConfig = supported.config();
    let channels = usize::from(config.channels.max(1));
    let sample_rate = config.sample_rate;
    let mut mono = Vec::<f32>::new();
    let err_fn = |e| tracing::warn!(error = %e, "output stream error");

    macro_rules! output_stream {
        ($t:ty) => {{
            dev.build_output_stream::<$t, _, _>(
                config,
                move |data: &mut [$t], _| {
                    let frames = data.len() / channels;
                    mono.resize(frames, 0.0);
                    fill(&mut mono);
                    for (frame, s) in data.chunks_exact_mut(channels).zip(mono.iter()) {
                        let v = <$t as cpal::Sample>::from_sample(*s);
                        frame.fill(v);
                    }
                },
                err_fn,
                None,
            )
        }};
    }
    let stream = match format {
        SampleFormat::F32 => output_stream!(f32),
        SampleFormat::I16 => output_stream!(i16),
        SampleFormat::U16 => output_stream!(u16),
        SampleFormat::I32 => output_stream!(i32),
        other => {
            return Err(MediaError::Device(format!(
                "unsupported output format {other:?}"
            )));
        }
    }
    .map_err(dev_err)?;
    Ok(Output {
        stream,
        sample_rate,
        channels,
    })
}
