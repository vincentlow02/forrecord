//! Windows capture adapted from MeetRec's capture/mod.rs and capture/windows.rs (MIT).

use crate::resample::downmix_to_mono_f32;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use serde::Serialize;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::time::Duration;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

#[derive(Clone, Copy)]
pub enum Source {
    Interviewer,
    Me,
}

impl Source {
    pub const ALL: [Self; 2] = [Self::Interviewer, Self::Me];

    pub fn label(self) -> &'static str {
        match self {
            Self::Interviewer => "Interviewer",
            Self::Me => "Me",
        }
    }

    fn default(self, host: &cpal::Host) -> Option<cpal::Device> {
        match self {
            Self::Interviewer => host.default_output_device(),
            Self::Me => host.default_input_device(),
        }
    }

    fn devices(self, host: &cpal::Host) -> Result<Vec<cpal::Device>, cpal::Error> {
        match self {
            Self::Interviewer => Ok(host.output_devices()?.collect()),
            Self::Me => Ok(host.input_devices()?.collect()),
        }
    }

    fn config(self, device: &cpal::Device) -> Result<cpal::SupportedStreamConfig, cpal::Error> {
        match self {
            Self::Interviewer => device.default_output_config(),
            Self::Me => device.default_input_config(),
        }
    }
}

#[derive(Clone, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub id: String,
    pub name: String,
}

fn describe(device: &cpal::Device) -> Result<Device, cpal::Error> {
    let id = device.id()?.to_string();
    let name = device
        .description()
        .map(|description| description.name().to_owned())
        .unwrap_or_else(|_| "Windows audio device".into());
    Ok(Device { id, name })
}

pub fn devices(source: Source) -> Result<Vec<Device>, String> {
    let host = cpal::default_host();
    let mut list: Vec<_> = source
        .devices(&host)
        .map_err(|error| format!("{} devices: {error}", source.label()))?
        .iter()
        .filter_map(|device| describe(device).ok())
        .collect();
    list.sort_by_cached_key(|device| device.name.to_lowercase());
    Ok(list)
}

pub struct Clock(u64);

impl Clock {
    pub fn new() -> Result<Self, String> {
        let mut frequency = 0;
        // WASAPI capture timestamps and Windows QPC share the same epoch.
        unsafe { QueryPerformanceFrequency(&mut frequency) }.map_err(|error| error.to_string())?;
        Ok(Self(frequency as u64))
    }

    pub fn now(&self) -> Duration {
        let mut ticks = 0;
        // QPC is guaranteed to succeed on supported Windows versions.
        unsafe { QueryPerformanceCounter(&mut ticks) }.expect("Windows monotonic clock");
        Duration::from_nanos((ticks as u128 * 1_000_000_000 / self.0 as u128) as u64)
    }
}

pub struct Packet {
    pub at: Duration,
    pub samples: Vec<f32>,
}

#[derive(Default)]
pub struct Health {
    fault: Mutex<Option<String>>,
    gaps: AtomicU32,
}

impl Health {
    fn report(&self, error: cpal::Error) {
        if error.kind() == cpal::ErrorKind::Xrun {
            self.gaps.fetch_add(1, Ordering::Relaxed);
        } else {
            self.fault
                .lock()
                .unwrap()
                .get_or_insert_with(|| error.to_string());
        }
    }

    pub fn fault(&self) -> Option<String> {
        self.fault.lock().unwrap().clone()
    }

    pub fn gaps(&self) -> u32 {
        self.gaps.load(Ordering::Relaxed)
    }
}

pub struct Capture {
    pub device: Device,
    pub rate: u32,
    pub channels: u16,
    pub format: String,
    pub packets: Receiver<Packet>,
    pub health: Arc<Health>,
    input: cpal::Stream,
    silent_render: Option<cpal::Stream>,
}

impl Capture {
    pub fn open(source: Source, selected: Option<&str>) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = match selected {
            None => source.default(&host),
            Some(id) => source
                .devices(&host)
                .map_err(|e| e.to_string())?
                .into_iter()
                .find(|device| device.id().is_ok_and(|actual| actual.to_string() == id)),
        }
        .ok_or_else(|| {
            format!(
                "{} device is unavailable. Check Settings and Windows audio.",
                source.label()
            )
        })?;
        let open = || -> Result<Self, cpal::Error> {
            let details = describe(&device)?;
            // CPAL uses the endpoint's native shared-mode format. For a render
            // endpoint, its WASAPI input stream automatically enables loopback.
            let supported = source.config(&device)?;
            let format = supported.sample_format();
            let config: cpal::StreamConfig = supported.into();
            let health = Arc::new(Health::default());
            let (send, packets) = sync_channel(96);
            let input = dispatch!(
                format,
                input_stream,
                &device,
                config,
                send,
                Arc::clone(&health)
            )?;
            // Keep the selected render endpoint active during silence, as MeetRec does.
            let silent_render = match source {
                Source::Interviewer => Some(dispatch!(
                    format,
                    render_stream,
                    &device,
                    config,
                    Arc::clone(&health)
                )?),
                Source::Me => None,
            };
            Ok(Self {
                device: details,
                rate: config.sample_rate,
                channels: config.channels,
                format: format.to_string(),
                packets,
                health,
                input,
                silent_render,
            })
        };
        open().map_err(|error| format!("{} capture: {error}", source.label()))
    }

    pub fn play(&self, source: Source) -> Result<(), String> {
        let play = || -> Result<(), cpal::Error> {
            self.input.play()?;
            if let Some(render) = &self.silent_render {
                render.play()?;
            }
            Ok(())
        };
        play().map_err(|error| format!("{} capture: {error}", source.label()))
    }
}

macro_rules! dispatch {
    ($format:expr, $build:ident, $($args:expr),+) => {
        match $format {
            SampleFormat::F32 => $build::<f32>($($args),+),
            SampleFormat::I16 => $build::<i16>($($args),+),
            SampleFormat::I24 => $build::<cpal::I24>($($args),+),
            SampleFormat::I32 => $build::<i32>($($args),+),
            SampleFormat::U8 => $build::<u8>($($args),+),
            format => Err(cpal::Error::with_message(
                cpal::ErrorKind::UnsupportedConfig, format!("Unsupported device sample format: {format}"),
            )),
        }
    };
}
use dispatch;

fn input_stream<T: SizedSample>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    sink: SyncSender<Packet>,
    health: Arc<Health>,
) -> Result<cpal::Stream, cpal::Error>
where
    f32: FromSample<T>,
{
    let callback_health = Arc::clone(&health);
    let mut last_end = Duration::ZERO;
    device.build_input_stream::<T, _, _>(
        config,
        move |data, info| {
            let timestamp = info.timestamp();
            let callback = Duration::from_nanos(timestamp.callback.as_nanos() as u64);
            let duration = Duration::from_secs_f64(
                (data.len() / config.channels as usize) as f64 / config.sample_rate as f64,
            );
            let mut at = Duration::from_nanos(timestamp.capture.as_nanos() as u64);
            if at.is_zero()
                || at > callback + Duration::from_millis(100)
                || callback.saturating_sub(at) > Duration::from_secs(2)
            {
                callback_health.gaps.fetch_add(1, Ordering::Relaxed);
                at = callback
                    .saturating_sub(duration)
                    .max(last_end.min(callback));
            }
            last_end = at + duration;
            let samples = downmix_to_mono_f32(data, config.channels);
            if let Err(TrySendError::Full(_)) = sink.try_send(Packet { at, samples }) {
                callback_health.gaps.fetch_add(1, Ordering::Relaxed);
            }
        },
        move |error| health.report(error),
        None,
    )
}

fn render_stream<T: SizedSample>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    health: Arc<Health>,
) -> Result<cpal::Stream, cpal::Error> {
    device.build_output_stream::<T, _, _>(
        config,
        |data, _| data.fill(T::EQUILIBRIUM),
        move |error| health.report(error),
        None,
    )
}
