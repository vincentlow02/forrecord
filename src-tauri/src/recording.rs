use crate::capture::{Capture, Clock, Packet, Source};
use crate::resample::Resampler;
use crate::storage::{
    Pause, Result, SAMPLE_RATE, SaveState, Session, SessionFiles, TRACK_FILES, TrackInfo, WavSink,
};
use chrono::Utc;
use serde::Serialize;
use std::collections::VecDeque;
use std::time::Duration;

#[cfg(test)]
#[path = "recording_tests.rs"]
mod tests;

const WRITE_LAG: Duration = Duration::from_millis(250);
const CHECKPOINT: Duration = Duration::from_secs(5);
const STALL: Duration = Duration::from_secs(3);
const MAX_PENDING: usize = 128;
const MAX_DURATION: Duration = Duration::from_secs(24 * 60 * 60);
const ZEROES: [i16; 8192] = [0; 8192];

fn frames(duration: Duration, rate: u32) -> u64 {
    (duration.as_nanos() * rate as u128 / 1_000_000_000) as u64
}

fn frame_duration(count: usize, rate: u32) -> Duration {
    Duration::from_nanos((count as u64 * 1_000_000_000).div_ceil(rate as u64))
}

#[derive(Clone, Copy)]
struct Segment {
    start: Duration,
    base: u64,
}

impl Segment {
    fn frame_at(self, time: Duration) -> u64 {
        self.base + frames(time.saturating_sub(self.start), SAMPLE_RATE)
    }
}

#[derive(Default)]
struct Timeline {
    active: Option<Segment>,
    recorded: u64,
}

impl Timeline {
    fn resume(&mut self, now: Duration) -> bool {
        if self.active.is_some() {
            return false;
        }
        self.active = Some(Segment {
            start: now,
            base: self.recorded,
        });
        true
    }

    fn pause(&mut self, now: Duration) -> Option<Segment> {
        let segment = self.active.take()?;
        self.recorded = segment.frame_at(now);
        Some(segment)
    }

    fn duration(&self, now: Duration) -> u64 {
        self.active
            .map_or(self.recorded, |segment| segment.frame_at(now))
    }
}

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackStatus {
    pub frames: u64,
    pub level: f32,
    pub device: Option<String>,
    pub issue: Option<String>,
}

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Idle,
    Recording,
    Paused,
    Saved,
    Interrupted,
    Testing,
}

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub phase: Phase,
    pub frames: u64,
    pub tracks: [TrackStatus; 2],
    pub message: Option<String>,
    pub bookmarks: usize,
}

pub struct Input {
    source: Source,
    device: Capture,
    last_packet: Duration,
    latest_level: f32,
    pub fault: Option<String>,
}

impl Input {
    pub fn open(source: Source, selection: Option<&str>) -> std::result::Result<Self, String> {
        Ok(Self {
            source,
            device: Capture::open(source, selection)?,
            last_packet: Duration::ZERO,
            latest_level: 0.0,
            fault: None,
        })
    }

    pub fn play(&mut self, now: Duration) -> std::result::Result<(), String> {
        self.device.play(self.source)?;
        self.last_packet = now;
        Ok(())
    }

    pub fn poll(&mut self, now: Duration, mut enqueue: impl FnMut(Packet) -> bool) {
        let mut level = 0.0f32;
        let mut overflow = false;
        for packet in self.device.packets.try_iter().take(96) {
            if packet.samples.is_empty() {
                continue;
            }
            self.last_packet = now;
            let square_sum = packet.samples.iter().map(|x| x * x).sum::<f32>();
            level = level.max((square_sum / packet.samples.len() as f32).sqrt());
            overflow |= !enqueue(packet);
        }
        self.latest_level = (level * 3.2)
            .sqrt()
            .clamp(0.0, 1.0)
            .max(self.latest_level * 0.67);
        let failure = self.device.health.fault().or_else(|| {
            if overflow {
                Some("Audio buffering could not keep up.".into())
            } else if now.saturating_sub(self.last_packet) > STALL {
                Some("The audio device stopped delivering samples.".into())
            } else {
                None
            }
        });
        if let Some(error) = failure {
            self.fault
                .get_or_insert_with(|| format!("{}: {error}", self.source.label()));
        }
    }

    pub fn status(&self, frames: u64, show_levels: bool, gaps: u32) -> TrackStatus {
        TrackStatus {
            frames,
            level: if show_levels && self.fault.is_none() {
                self.latest_level
            } else {
                0.0
            },
            device: Some(self.device.device.name.clone()),
            issue: self
                .fault
                .clone()
                .or_else(|| (gaps > 0).then(|| format!("{gaps} capture gaps reported"))),
        }
    }

    fn info(&self, index: usize) -> TrackInfo {
        TrackInfo {
            path: TRACK_FILES[index].into(),
            device: self.device.device.clone(),
            source_rate: self.device.rate,
            source_channels: self.device.channels,
            source_format: self.device.format.clone(),
            captured_frames: 0,
            last_capture_frame: 0,
            discontinuities: 0,
        }
    }
}

struct AlignedTrack {
    rate: u32,
    pending: VecDeque<Packet>,
    resampler: Resampler,
    cursor: u64,
    last_audio: u64,
    captured: u64,
}

impl AlignedTrack {
    fn new(rate: u32) -> Self {
        Self {
            rate,
            pending: VecDeque::new(),
            resampler: Resampler::new(rate, SAMPLE_RATE),
            cursor: 0,
            last_audio: 0,
            captured: 0,
        }
    }

    fn silence_to(&mut self, end: u64, sink: &mut WavSink) -> Result<()> {
        while self.cursor < end {
            let count = (end - self.cursor).min(ZEROES.len() as u64) as usize;
            sink.write(&ZEROES[..count])?;
            self.cursor += count as u64;
        }
        Ok(())
    }

    fn fill(
        &mut self,
        segment: Segment,
        until: Duration,
        closing: bool,
        sink: &mut WavSink,
    ) -> Result<()> {
        let end = segment.frame_at(until);
        while let Some(packet) = self.pending.front() {
            if packet.at >= until {
                break;
            }
            if !closing && packet.at + frame_duration(packet.samples.len(), self.rate) > until {
                // The next packet straddles the write watermark. Keep it intact until the next tick.
                return Ok(());
            }
            let packet = self.pending.pop_front().unwrap();
            let first = if packet.at < segment.start {
                // Round up so that a sample from before Start/Resume cannot enter the FIR history.
                (segment.start.saturating_sub(packet.at).as_nanos() * self.rate as u128)
                    .div_ceil(1_000_000_000) as usize
            } else {
                0
            };
            let last = packet.samples.len().min(
                ((until - packet.at).as_nanos() * self.rate as u128).div_ceil(1_000_000_000)
                    as usize,
            );
            if first >= last {
                continue;
            }
            let at = packet.at + frame_duration(first, self.rate);
            let nominal = segment.frame_at(at);
            // Reconcile driver jitter to the end of the last contiguous packet. Correct
            // larger gaps and accumulated clock drift against the shared QPC timeline.
            let contiguous = self.last_audio == self.cursor && nominal.abs_diff(self.cursor) <= 16;
            let offset = if contiguous { self.cursor } else { nominal };
            if offset > self.cursor {
                self.silence_to(offset.min(end), sink)?;
                self.resampler.reset();
            }
            let data = self.resampler.process_to_i16(&packet.samples[first..last]);
            let skip = self.cursor.saturating_sub(offset).min(data.len() as u64) as usize;
            let count = (end - self.cursor).min((data.len() - skip) as u64) as usize;
            if count > 0 {
                sink.write(&data[skip..skip + count])?;
                self.cursor += count as u64;
                self.last_audio = self.cursor;
                self.captured += count as u64;
            }
        }
        self.silence_to(end, sink)?;
        Ok(())
    }
}

pub struct Recording {
    inputs: [Input; 2],
    aligned: [AlignedTrack; 2],
    files: SessionFiles,
    timeline: Timeline,
    next_checkpoint: Duration,
    gaps: [u32; 2],
    previous_gaps: [u32; 2],
    last_tick: Duration,
}

impl Recording {
    pub fn start(mut inputs: [Input; 2], root: &std::path::Path, clock: &Clock) -> Result<Self> {
        let mut files = SessionFiles::create(root, [inputs[0].info(0), inputs[1].info(1)])?;
        let now = clock.now();
        for input in &mut inputs {
            if let Err(error) = input.play(now) {
                let folder = files.folder.clone();
                drop(files);
                let _ = std::fs::remove_dir_all(folder);
                return Err(error.into());
            }
        }
        let start = clock.now();
        files.session.started_at = Utc::now();
        let aligned = inputs
            .each_ref()
            .map(|input| AlignedTrack::new(input.device.rate));
        let mut timeline = Timeline::default();
        timeline.resume(start);
        Ok(Self {
            inputs,
            aligned,
            files,
            timeline,
            next_checkpoint: start + CHECKPOINT,
            gaps: [0; 2],
            previous_gaps: [0; 2],
            last_tick: start,
        })
    }

    fn poll(&mut self, now: Duration, active: bool) {
        for (index, (input, track)) in self.inputs.iter_mut().zip(&mut self.aligned).enumerate() {
            input.poll(now, |packet| {
                if !active {
                    return true;
                }
                if track.pending.len() == MAX_PENDING {
                    return false;
                }
                track.pending.push_back(packet);
                true
            });
            let total = input.device.health.gaps();
            if active {
                self.gaps[index] = self.gaps[index]
                    .saturating_add(total.saturating_sub(self.previous_gaps[index]));
            }
            self.previous_gaps[index] = total;
        }
    }

    pub fn tick(&mut self, now: Duration) -> Result<Option<String>> {
        if now.saturating_sub(self.last_tick) > STALL && self.timeline.active.is_some() {
            return Ok(Some("Windows interrupted the recording. Audio was saved through the last capture window.".into()));
        }
        self.last_tick = now;
        let segment = self.timeline.active;
        self.poll(now, segment.is_some());
        if let Some(segment) = segment {
            self.fill(
                segment,
                now.saturating_sub(WRITE_LAG).max(segment.start),
                false,
            )?;
        }
        if now >= self.next_checkpoint {
            self.checkpoint()?;
            self.next_checkpoint = now + CHECKPOINT;
        }
        let fault = self
            .inputs
            .iter()
            .filter_map(|input| input.fault.clone())
            .collect::<Vec<_>>();
        if !fault.is_empty() {
            return Ok(Some(fault.join(" · ")));
        }
        Ok(
            (self.timeline.duration(now) >= frames(MAX_DURATION, SAMPLE_RATE))
                .then(|| "Reached the 24-hour session limit.".into()),
        )
    }

    fn fill(&mut self, segment: Segment, until: Duration, closing: bool) -> Result<()> {
        for (track, writer) in self.aligned.iter_mut().zip(&mut self.files.writers) {
            track.fill(segment, until, closing, writer)?;
        }
        Ok(())
    }

    fn close_segment(&mut self, now: Duration, clock: &Clock) -> Result<bool> {
        let Some(segment) = self.timeline.pause(now) else {
            return Ok(false);
        };
        // Let the final WASAPI packets arrive, then cut both tracks at the same sample boundary.
        std::thread::sleep(WRITE_LAG);
        self.poll(clock.now(), true);
        self.fill(segment, now, true)?;
        for track in &mut self.aligned {
            track.pending.clear();
            track.resampler.reset();
        }
        Ok(true)
    }

    pub fn pause(&mut self, clock: &Clock) -> Result<()> {
        let paused_at = Utc::now();
        if self.close_segment(clock.now(), clock)? {
            self.files.session.pauses.push(Pause {
                at_frame: self.timeline.recorded,
                paused_at,
                resumed_at: None,
            });
            self.checkpoint()?;
        }
        Ok(())
    }

    pub fn resume(&mut self, clock: &Clock) -> Result<()> {
        self.last_tick = clock.now();
        if self.timeline.resume(self.last_tick)
            && let Some(pause) = self.files.session.pauses.last_mut()
        {
            pause.resumed_at = Some(Utc::now());
            self.files.session.save(&self.files.folder)?;
        }
        Ok(())
    }

    pub fn bookmark(&mut self, now: Duration) -> Result<()> {
        self.files
            .session
            .bookmarks
            .push(self.timeline.duration(now));
        self.files.session.save(&self.files.folder)
    }

    fn update_metadata(&mut self) {
        self.files.session.duration_frames =
            self.aligned.iter().map(|track| track.cursor).min().unwrap();
        for (index, track) in self.aligned.iter().enumerate() {
            let info = &mut self.files.session.tracks[index];
            info.last_capture_frame = track.last_audio;
            info.captured_frames = track.captured;
            info.discontinuities = self.gaps[index];
        }
    }

    fn checkpoint(&mut self) -> Result<()> {
        for writer in &mut self.files.writers {
            writer.checkpoint()?;
        }
        self.update_metadata();
        self.files.session.save(&self.files.folder)
    }

    pub fn snapshot(&self, now: Duration) -> Snapshot {
        let frames = self.timeline.duration(now);
        let active = self.timeline.active.is_some();
        Snapshot {
            phase: if active {
                Phase::Recording
            } else {
                Phase::Paused
            },
            frames,
            tracks: std::array::from_fn(|i| {
                let count = if self.inputs[i].fault.is_some() {
                    self.aligned[i].last_audio
                } else {
                    frames
                };
                self.inputs[i].status(count, active, self.gaps[i])
            }),
            message: None,
            bookmarks: self.files.session.bookmarks.len(),
        }
    }

    pub fn finish(
        mut self,
        clock: &Clock,
        reason: Option<String>,
        write_failed: bool,
    ) -> (Snapshot, Result<Session>) {
        let now = clock.now().min(self.last_tick + WRITE_LAG);
        let mut issues: Vec<_> = reason.into_iter().collect();
        let mut salvage = write_failed;
        if !salvage && let Err(error) = self.close_segment(now, clock) {
            salvage = true;
            issues.push(format!("Could not finish writing WAV files: {error}"));
        }
        for input in &self.inputs {
            if let Some(fault) = &input.fault
                && !issues.iter().any(|issue| issue.contains(fault))
            {
                issues.push(fault.clone());
            }
        }
        self.update_metadata();
        let mut snapshot = self.snapshot(now);
        let SessionFiles {
            folder,
            mut session,
            lock,
            writers,
        } = self.files;
        let failed_tracks = self.inputs.each_ref().map(|input| input.fault.is_some());
        drop(self.inputs);
        for (index, sink) in writers.into_iter().enumerate() {
            if let Err(error) = sink.finish() {
                salvage = true;
                issues.push(format!(
                    "Could not finalize {}: {error}",
                    TRACK_FILES[index]
                ));
            }
        }
        session.ended_at = Some(Utc::now());
        session.issues = issues;
        session.state = if session.issues.is_empty() {
            SaveState::Saved
        } else {
            SaveState::Interrupted
        };
        let result = if salvage {
            crate::storage::salvage(&folder, session)
        } else {
            session.save(&folder).map(|()| session)
        };
        drop(lock);
        match &result {
            Ok(session) => {
                snapshot.frames = session.duration_frames;
                snapshot.phase = if session.issues.is_empty() {
                    Phase::Saved
                } else {
                    Phase::Interrupted
                };
                snapshot.message = (!session.issues.is_empty()).then(|| session.issues.join(" · "));
                for ((track, info), failed) in snapshot
                    .tracks
                    .iter_mut()
                    .zip(&session.tracks)
                    .zip(failed_tracks)
                {
                    track.frames = if failed {
                        info.last_capture_frame.min(session.duration_frames)
                    } else {
                        session.duration_frames
                    };
                }
            }
            Err(error) => {
                snapshot.phase = Phase::Interrupted;
                snapshot.message = Some(format!(
                    "Recording stopped. Could not finish saving: {error}. Check the recordings folder and reopen History to try recovery.",
                ));
            }
        }
        for track in &mut snapshot.tracks {
            track.level = 0.0;
        }
        (snapshot, result)
    }
}

pub struct AudioTest {
    pub inputs: [Option<Input>; 2],
    pub errors: [Option<String>; 2],
    pub expires: Duration,
}

impl AudioTest {
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            phase: Phase::Testing,
            tracks: std::array::from_fn(|i| match &self.inputs[i] {
                Some(input) => input.status(0, true, 0),
                None => TrackStatus {
                    issue: self.errors[i].clone(),
                    ..TrackStatus::default()
                },
            }),
            ..Snapshot::default()
        }
    }
}
