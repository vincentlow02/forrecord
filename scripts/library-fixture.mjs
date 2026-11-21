import { mkdir, rmdir, unlink, writeFile } from "node:fs/promises";
import { resolve, join } from "node:path";

const [action, root, id] = process.argv.slice(2);
if (!/^[0-9a-f]{32}$/u.test(id)) throw new Error("Invalid fixture ID.");
const folder = join(resolve(root), id);
const names = ["interviewer.wav", "me.wav"];
const rate = 16_000;
const durationFrames = rate * 44;

function wav(frequency) {
  const bytes = Buffer.alloc(44 + durationFrames * 2);
  bytes.write("RIFF", 0);
  bytes.writeUInt32LE(bytes.length - 8, 4);
  bytes.write("WAVEfmt ", 8);
  bytes.writeUInt32LE(16, 16);
  bytes.writeUInt16LE(1, 20);
  bytes.writeUInt16LE(1, 22);
  bytes.writeUInt32LE(rate, 24);
  bytes.writeUInt32LE(rate * 2, 28);
  bytes.writeUInt16LE(2, 32);
  bytes.writeUInt16LE(16, 34);
  bytes.write("data", 36);
  bytes.writeUInt32LE(durationFrames * 2, 40);
  for (let frame = 0; frame < durationFrames; frame++) {
    bytes.writeInt16LE(Math.round(Math.sin(frame * 2 * Math.PI * frequency / rate) * 800), 44 + frame * 2);
  }
  return bytes;
}

if (action === "create") {
  await mkdir(resolve(root), { recursive: true });
  await mkdir(folder);
  for (const [index, name] of names.entries()) await writeFile(join(folder, name), wav(330 + index * 220), { flag: "wx" });
  await writeFile(join(folder, "session.json"), JSON.stringify({
    id,
    name: "NexQ QA legacy fixture",
    startedAt: "2026-01-01T10:00:00Z",
    endedAt: "2026-01-01T10:00:44Z",
    state: "saved",
    durationFrames,
    sampleRate: rate,
    channels: 1,
    bitsPerSample: 16,
    synchronization: "Fixture",
    tracks: names.map((path) => ({
      path, device: { id: "fixture", name: "QA fixture" }, sourceRate: rate,
      sourceChannels: 1, sourceFormat: "i16", capturedFrames: durationFrames,
      lastCaptureFrame: durationFrames, discontinuities: 0,
    })),
    pauses: [],
    bookmarks: [rate * 15],
    issues: [],
  }), { flag: "wx" });
} else if (action === "remove") {
  for (const name of [...names, "session.json"]) {
    await unlink(join(folder, name)).catch((error) => { if (error.code !== "ENOENT") throw error; });
  }
  await rmdir(folder).catch((error) => { if (error.code !== "ENOENT") throw error; });
} else {
  throw new Error("Unknown fixture action.");
}
