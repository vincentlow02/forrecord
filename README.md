# NexQ Recorder

Simple, private, dual-track audio recording for Windows.

One button records **Interviewer** (your selected Windows playback device via WASAPI loopback) and **Me** (your selected microphone). Both tracks have live audio meters, duration counters, and their own WAV files. Pausing removes the paused interval from both tracks. Bookmarks save timestamps in the session metadata.

![NexQ Recorder on Windows, ready to record both tracks](docs/recorder.png)

## Install and record

1. Open a successful **Windows build** run and download the `nexq-recorder-windows-x64` artifact. Extract it and run `NexQ Recorder_0.1.0_x64-setup.exe`. For portable use, run `nexq-recorder.exe` on a machine with the Microsoft Edge WebView2 Runtime. [GitHub Actions](https://github.com/vincentlow02/forrecord/actions/workflows/windows.yml)
2. In **Settings**, choose the microphone, the device that plays your meeting sound, and a recording folder. **Test devices** checks live audio levels for up to 30 seconds without creating any files.
3. Click the red microphone button. Use the side controls to pause, resume, or bookmark. Click the red square to save.
4. Open **Recordings** to play either track in the Windows default audio player, open its folder, rename it, copy the session, or delete it.

The installer is unsigned and includes WebView2 for offline installation. Windows 10 22H2 and Windows 11 x64 are the target platforms. Windows must allow desktop apps to access the microphone.

## Audio and local files

- Keep the call's audio routed to the selected playback device. Loopback records the device mix, including other apps and notification sounds. Headphones reduce acoustic leakage into the microphone.
- The default recording location is `%LOCALAPPDATA%\com.nexq.recorder\Recordings`. Settings and recordings stay on the computer. If you choose a synced or network folder, Windows or your sync software may copy the files.
- Each session folder has `interviewer.wav`, `me.wav`, and `session.json`. WAVs are mono, 16-bit PCM at 16 kHz. Together they use about 230 MB per hour. Metadata records UTC timestamps, device and format details, pause and bookmark positions, and capture issues.
- The recording worker uses WASAPI's shared monotonic clock for both tracks and fills missing intervals with silence. If a device fails, NexQ reports which track failed and stops to save the audio captured so far. Reopen **Recordings** after an unexpected shutdown to recover the common playable length from the files that reached disk. The last few seconds may be missing after power loss.
- A session can run for up to 24 hours, subject to available disk space and stable audio devices. Both endpoints stay open during a pause to avoid losing the selected device. Paused audio is excluded from the WAVs and the timers.

## Build from source

Install Node.js 24, Rust 1.100 with the `x86_64-pc-windows-msvc` target, Visual Studio Build Tools with **Desktop development with C++** and a Windows SDK, and the Microsoft Edge WebView2 Runtime. Run the following commands in an x64 Developer PowerShell. [Tauri Windows prerequisites](https://v2.tauri.app/start/prerequisites/)

```powershell
npm ci
npm run check
cargo test --release --locked --manifest-path src-tauri/Cargo.toml
npm run build
```

The frontend is bundled directly from `ui/`. The build writes:

- Portable executable at `src-tauri/target/release/nexq-recorder.exe`
- Offline installer at `src-tauri/target/release/bundle/nsis/NexQ Recorder_0.1.0_x64-setup.exe`

For development use `npm run tauri -- dev`. See [CONTRIBUTING.md](CONTRIBUTING.md) for hardware validation and versioned packaging.

## Attribution

NexQ Recorder is MIT licensed. Its native-format WASAPI capture, silent render stream, downmix, and polyphase resampler are adapted from MeetRec by Mikhail Maksimov and contributors. The app also follows MeetRec's separate streaming WAV writer and dedicated audio-worker architecture. The upstream copyright and MIT license are included in [LICENSE](LICENSE). [MeetRec source revision](https://github.com/mmaximov97/meeting-recorder/tree/41264c52b0e1c8efdd4c003b190bd0c57e479abc)
