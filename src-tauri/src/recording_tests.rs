use super::*;
use std::path::Path;

fn ms(milliseconds: u64) -> Duration {
    Duration::from_millis(milliseconds)
}

fn packet(at: u64, length_ms: usize, level: f32) -> Packet {
    Packet {
        at: ms(at),
        samples: vec![level; length_ms * 16],
    }
}

fn read_wav(path: &Path) -> Vec<i16> {
    let mut file = hound::WavReader::open(path).unwrap();
    assert_eq!(file.spec().channels, 1);
    assert_eq!(file.spec().sample_rate, SAMPLE_RATE);
    assert_eq!(file.spec().bits_per_sample, 16);
    file.samples::<i16>()
        .map(|sample| sample.unwrap())
        .collect()
}

#[test]
fn shared_timer_does_not_count_paused_time_or_duplicate_transitions() {
    let mut clock = Timeline::default();
    assert!(clock.pause(ms(5)).is_none());
    assert!(clock.resume(ms(100)));
    assert!(!clock.resume(ms(200)));
    assert_eq!(clock.duration(ms(300)), 3_200);
    assert!(clock.pause(ms(350)).is_some());
    assert_eq!(clock.duration(ms(2_000)), 4_000);
    assert!(clock.resume(ms(2_000)));
    assert_eq!(clock.duration(ms(2_250)), 8_000);
}

#[test]
fn writes_two_aligned_wavs_with_silence_and_exact_pause_boundaries() {
    let directory = tempfile::tempdir().unwrap();
    let paths = ["a.wav", "b.wav"].map(|name| directory.path().join(name));
    let mut writers = paths.each_ref().map(|path| WavSink::create(path).unwrap());
    let mut tracks = [
        AlignedTrack::new(SAMPLE_RATE),
        AlignedTrack::new(SAMPLE_RATE),
    ];
    let mut clock = Timeline::default();
    clock.resume(ms(100));
    let first = clock.active.unwrap();
    // The interviewer starts late and its packet extends into the pause.
    tracks[0].pending.push_back(packet(175, 120, 0.25));
    // The microphone packet starts before the session opens.
    tracks[1].pending.push_back(packet(90, 35, 0.5));
    for (track, writer) in tracks.iter_mut().zip(&mut writers) {
        track.fill(first, ms(200), false, writer).unwrap();
        track.fill(first, ms(250), true, writer).unwrap();
    }
    clock.pause(ms(250));
    clock.resume(ms(700));
    let second = clock.active.unwrap();
    tracks[0].pending.push_back(packet(680, 55, -0.25));
    tracks[1].pending.push_back(packet(705, 30, -0.5));
    for (track, writer) in tracks.iter_mut().zip(&mut writers) {
        track.fill(second, ms(800), true, writer).unwrap();
        assert_eq!(track.cursor, 4_000);
    }
    for writer in writers {
        writer.finish().unwrap();
    }
    let [a, b] = paths.each_ref().map(|path| read_wav(path));
    assert_eq!(a.len(), b.len());
    assert_eq!(a.len(), 4_000);
    assert!(a[..1200].iter().all(|&sample| sample == 0));
    assert!(a[1200..2400].iter().all(|&sample| sample == 8191));
    assert!(a[2400..2960].iter().all(|&sample| sample == -8191));
    assert!(a[2960..].iter().all(|&sample| sample == 0));
    assert!(b[..400].iter().all(|&sample| sample == 16383));
    assert!(b[400..2480].iter().all(|&sample| sample == 0));
    assert!(b[2480..2960].iter().all(|&sample| sample == -16383));
    assert!(b[2960..].iter().all(|&sample| sample == 0));
}

#[test]
fn writer_fills_a_timed_gap_after_a_dropped_packet() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("gap.wav");
    let mut writer = WavSink::create(&path).unwrap();
    let mut track = AlignedTrack::new(SAMPLE_RATE);
    let segment = Segment {
        start: ms(100),
        base: 0,
    };
    track
        .pending
        .extend([packet(100, 10, 0.5), packet(120, 10, -0.5)]);
    track.fill(segment, ms(130), true, &mut writer).unwrap();
    writer.finish().unwrap();
    assert_eq!(track.captured, 320);
    let samples = read_wav(&path);
    assert_eq!(samples.len(), 480);
    assert!(samples[160..320].iter().all(|&sample| sample == 0));
    assert_ne!(samples[159], 0);
    assert_ne!(samples[320], 0);
}

#[test]
#[ignore = "Writes about 922 MB of synthetic audio. Run explicitly for release validation."]
fn streams_four_hours_into_two_standard_wavs() {
    let directory = tempfile::tempdir().unwrap();
    let duration = Duration::from_secs(4 * 3600);
    let segment = Segment {
        start: Duration::ZERO,
        base: 0,
    };
    let expected = frames(duration, SAMPLE_RATE);
    for name in TRACK_FILES {
        let path = directory.path().join(name);
        let mut sink = WavSink::create(&path).unwrap();
        let mut track = AlignedTrack::new(SAMPLE_RATE);
        for minute in 1..=240 {
            track
                .fill(segment, Duration::from_secs(minute * 60), false, &mut sink)
                .unwrap();
        }
        sink.finish().unwrap();
        let mut reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.duration() as u64, expected);
        reader.seek((expected - 1) as u32).unwrap();
        assert_eq!(reader.samples::<i16>().next().unwrap().unwrap(), 0);
        assert_eq!(std::fs::metadata(path).unwrap().len(), 44 + expected * 2);
    }
}
