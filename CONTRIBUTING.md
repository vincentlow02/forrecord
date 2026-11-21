# Contributing

Use Windows x64 and the prerequisites in [README.md](README.md). Keep changes small and run the same checks as CI.

```powershell
npm ci
npm run check
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo clippy --release --locked --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --release --locked --manifest-path src-tauri/Cargo.toml
npm run build
```

Rust tests cover sample formats, resampling, pause boundaries, aligned WAVs, silence padding, metadata, recovery, and library operations. The extended synthetic check writes about 922 MB to the system temporary directory.

```powershell
cargo test --release --locked --manifest-path src-tauri/Cargo.toml streams_four_hours_into_two_standard_wavs -- --ignored
./scripts/smoke-windows.ps1 -Executable ./src-tauri/target/release/nexq-recorder.exe
```

## Hardware checks

1. Use headphones and select the meeting's actual playback device. Speak into the microphone with system audio silent. Verify response on **Me** and listen to `me.wav`.
2. Play speech with the microphone quiet. Verify response on **Interviewer** and listen to `interviewer.wav`.
3. Speak over playback, pause while both sources have audio, resume, and stop. Check that both timers hold during the pause, both WAVs have identical lengths, and the paused section is absent.
4. Disconnect the selected microphone or output during a recording. Check that NexQ names the failing track and the interrupted session appears in **Recordings**.
5. For a four-hour hardware run, leave at least 1 GB free. Check memory, CPU, track continuity, and synchronization near the end. Repeat with Bluetooth or USB hardware if you use it.
6. Resize the app at 125%, 150%, and 200% scale and check that the recorder controls, both cards, and dialog navigation remain usable.

Use test audio in shared reports. Keep any real recordings, personal settings, and device IDs local.

## Windows release

Set the same version in `package.json`, `package-lock.json`, `src-tauri/Cargo.toml`, `src-tauri/Cargo.lock`, and `src-tauri/tauri.conf.json`. The Windows workflow packages `main` and version tags, validates installation and the installed WebView, then uploads the installer, SHA-256 checksum, and portable executable to the Actions run. Use the green build for the release assets. Include the hardware checks performed, any gaps, and the unsigned installer status in the release notes.
