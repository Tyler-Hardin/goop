// stt.js — Speech-to-text recording module
//
// Exports three functions for the Rust (wasm-bindgen) side:
//   sttStart()   → Promise<void>        (throws on mic denied / no support)
//   sttStop(cancelled: bool) → Promise<ArrayBuffer | null>
//   sttState     → "idle" | "recording"
//
// The Rust side reads `sttState` to guard against double-starts / races.

export let sttState = "idle";

// ── internal state ───────────────────────────────────────────────────

let stream = null;
let recorder = null;
/** @type {(blob: Blob) => void} */
let blobResolve = null;

// ── helpers ──────────────────────────────────────────────────────────

async function pickBestMic() {
  let devices;
  try {
    devices = await navigator.mediaDevices.enumerateDevices();
  } catch {
    return null;
  }
  const inputs = devices.filter((d) => d.kind === "audioinput");
  if (inputs.length === 0) return null;
  const skip = /earpiece|handset|headset|communications/i;
  const prefer = /speakerphone|speaker/i;
  for (const d of inputs) {
    if (d.label && prefer.test(d.label) && !skip.test(d.label))
      return d.deviceId;
  }
  for (const d of inputs) {
    if (d.label && !skip.test(d.label)) return d.deviceId;
  }
  for (const d of inputs) {
    if (!d.label) return d.deviceId;
  }
  return inputs[0].deviceId;
}

async function ensureStream() {
  if (stream) return stream;
  const deviceId = await pickBestMic();
  const constraints = {
    audio: {
      channelCount: 1,
      sampleRate: 16000,
      echoCancellation: true,
      noiseSuppression: true,
    },
  };
  if (deviceId) constraints.audio.deviceId = { ideal: deviceId };
  stream = await navigator.mediaDevices.getUserMedia(constraints);
  return stream;
}

function pcmToWav(samples, sampleRate) {
  const numChannels = 1;
  const bitsPerSample = 16;
  const byteRate = sampleRate * numChannels * (bitsPerSample / 8);
  const blockAlign = numChannels * (bitsPerSample / 8);
  const dataSize = samples.length * (bitsPerSample / 8);
  const buf = new ArrayBuffer(44 + dataSize);
  const view = new DataView(buf);

  function writeStr(off, s) {
    for (let i = 0; i < s.length; i++) view.setUint8(off + i, s.charCodeAt(i));
  }

  writeStr(0, "RIFF");
  view.setUint32(4, 36 + dataSize, true);
  writeStr(8, "WAVE");
  writeStr(12, "fmt ");
  view.setUint32(16, 16, true);
  view.setUint16(20, 1, true);
  view.setUint16(22, numChannels, true);
  view.setUint32(24, sampleRate, true);
  view.setUint32(28, byteRate, true);
  view.setUint16(32, blockAlign, true);
  view.setUint16(34, bitsPerSample, true);
  writeStr(36, "data");
  view.setUint32(40, dataSize, true);

  for (let i = 0; i < samples.length; i++) {
    const s = Math.max(-1, Math.min(1, samples[i]));
    view.setInt16(44 + i * 2, s < 0 ? s * 0x8000 : s * 0x7fff, true);
  }
  return buf;
}

// ── exported API ─────────────────────────────────────────────────────

/**
 * Start recording from the microphone.
 * Returns a Promise that resolves when the recorder has started,
 * or rejects with an Error if mic access is denied / unsupported.
 */
export async function sttStart() {
  if (sttState !== "idle") return;
  sttState = "recording";

  try {
    const s = await ensureStream();
    // If stop() was called while we were waiting for the mic, bail.
    if (sttState !== "recording") return;

    let mimeType = "";
    for (const m of ["audio/webm", "audio/mp4", "audio/ogg"]) {
      if (MediaRecorder.isTypeSupported(m)) {
        mimeType = m;
        break;
      }
    }
    if (!mimeType) {
      sttState = "idle";
      throw new Error("MediaRecorder not supported in this browser.");
    }

    recorder = new MediaRecorder(s, { mimeType });

    // Set up a promise that resolves when recorder.ondataavailable fires.
    const blobReady = new Promise((resolve) => {
      blobResolve = resolve;
    });
    recorder.ondataavailable = (ev) => {
      blobResolve(ev.data);
    };

    recorder.start();
  } catch (err) {
    sttState = "idle";
    throw err;
  }
}

/**
 * Stop recording.
 *
 * @param {boolean} cancelled — if true, discard the recording.
 * @returns {Promise<ArrayBuffer | null>} WAV data, or null if cancelled / empty / error.
 */
export async function sttStop(cancelled) {
  // If start() hasn't finished yet (mic prompt still up), just reset.
  if (!recorder) {
    sttState = "idle";
    return null;
  }

  // Stop the recorder and wait for the blob.
  const blob = await new Promise((resolve) => {
    recorder.ondataavailable = (ev) => resolve(ev.data);
    recorder.stop();
  });
  recorder = null;
  sttState = "idle";

  if (cancelled || !blob || blob.size === 0) {
    return null;
  }

  // Decode the compressed blob to raw PCM via the browser's native decoder.
  const ctx = new (window.AudioContext || window.webkitAudioContext)({
    sampleRate: 16000,
  });
  const arrayBuf = await blob.arrayBuffer();
  let audioBuf;
  try {
    audioBuf = await ctx.decodeAudioData(arrayBuf);
  } catch (err) {
    console.error("STT decode error:", err);
    ctx.close();
    return null;
  }
  ctx.close();

  const channelData = audioBuf.getChannelData(0);
  console.log(
    "STT: rate =",
    audioBuf.sampleRate,
    "Hz, samples =",
    channelData.length,
    "duration =",
    (channelData.length / audioBuf.sampleRate).toFixed(2),
    "s",
  );

  return pcmToWav(channelData, audioBuf.sampleRate);
}

/**
 * True while the recorder is active.
 */
export function sttIsRecording() {
  return sttState === "recording";
}
