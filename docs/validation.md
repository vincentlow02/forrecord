# Windows validation

## Recording reliability

V0.1's `discontinuities` field counted three separate events together. A driver `Xrun` reported an endpoint discontinuity. An invalid timestamp was replaced with an estimate and the packet was still queued. A full queue discarded the packet. The earlier three microphone warnings did not retain the event types, so they cannot establish how much audio was lost or delayed.

The recorder now records the categories separately and accounts for output frames inserted as silence or trimmed from overlapping packets. The worker previously committed trailing silence when no next packet was queued, which could replace a delayed packet's valid audio. The regression test now exercises that late-arrival path. Recovery keeps the longest usable original and logs any inserted silence or unrepairable track. Diagnostics contain counts and durations, with no audio samples in the logs.

## Desktop hardware run

One 60.378-second run used the physical Realtek Speakers loopback and Microphone Array on Windows 11 Pro x64 (build 26200). Both sources provided 48 kHz input and both saved WAVs contained 966,052 frames at 16 kHz.

| Measurement | Interviewer | Me |
| --- | ---: | ---: |
| Driver discontinuities | 0 | 0 |
| Playback keepalive discontinuities | 0 | 0 |
| Estimated timestamps | 0 | 0 |
| Source frames dropped by capture queue | 0 | 0 |
| Output frames padded with silence | 131 (8.19 ms) | 22 (1.38 ms) |
| Output overlap trimmed | 30 (1.88 ms) | 0 |

Generated tones before pause, after resume, and near the end appeared in the system WAV within 72–175 ms of their application markers, including WebView and Windows playback latency and 50 ms measurement windows. The pause marker was below the detection threshold in both files. The microphone endpoint ran for the full test and captured only a weak copy of the generated tones. Isolated microphone speech quality and spoken-track synchronization remain unverified.

## Coverage still needed for stable release

- Isolated spoken microphone audio and simultaneous speakers in a real call, including variance across headphones, USB, and Bluetooth devices.
- Physically disconnecting and reconnecting the selected input and output during recording.
- A four-hour live hardware soak with continuity and synchronization checks near the end.
- Native Windows 10 install and live audio capture.
- Native high-DPI settings and click-through of Save As and folder-picker cancellation on additional Windows systems.

The release candidate makes the installer public while these tests remain open.
