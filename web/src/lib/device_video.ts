// Live device video: H.264 Annex B access units from the device socket, decoded with
// WebCodecs onto a canvas. One unit per message; the stream's first carries SPS/PPS and an IDR.

/** The browser decodes H.264 (WebCodecs needs a secure context: https or localhost). */
export const can_decode_video = () => typeof VideoDecoder !== 'undefined';

/** Whether an access unit is a key frame, and its SPS (from the NAL header byte on). */
function scan(unit: Uint8Array): { key: boolean; sps: Uint8Array | null } {
  let key = false;
  let sps: Uint8Array | null = null;
  for (let i = 0; i + 3 < unit.length; i++) {
    if (unit[i] !== 0 || unit[i + 1] !== 0 || unit[i + 2] !== 1) continue;
    const type = unit[i + 3] & 0x1f;
    if (type === 5) key = true;
    if (type === 7) sps = unit.subarray(i + 3, i + 7);
    i += 3;
  }
  return { key, sps };
}

const hex = (n: number) => n.toString(16).padStart(2, '0');

/** Decodes units onto `canvas` (sized to the coded picture = device pixels). `on_frame` fires per
 *  picture shown; `on_error` once when decoding fails (the caller falls back to pictures). */
export function video_player(canvas: HTMLCanvasElement, on_frame: () => void, on_error: (e: Error) => void) {
  const ctx = canvas.getContext('2d');
  let decoder: VideoDecoder | null = null;
  let codec = '';
  let count = 0;
  let failed = false;

  const close = () => {
    if (decoder && decoder.state !== 'closed') decoder.close();
  };
  const fail = (e: Error) => {
    if (failed) return;
    failed = true;
    close();
    on_error(e);
  };
  const draw = (frame: VideoFrame) => {
    if (canvas.width !== frame.displayWidth) canvas.width = frame.displayWidth;
    if (canvas.height !== frame.displayHeight) canvas.height = frame.displayHeight;
    ctx?.drawImage(frame, 0, 0);
    frame.close();
    on_frame();
  };

  return {
    push(data: ArrayBuffer) {
      if (failed) return;
      const unit = new Uint8Array(data);
      const { key, sps } = scan(unit);
      // avc1.PPCCLL: profile_idc, constraint flags, level_idc. A new one (a new stream) reconfigures.
      const wanted = key && sps ? `avc1.${hex(sps[1])}${hex(sps[2])}${hex(sps[3])}` : codec;
      try {
        if (wanted && wanted !== codec) {
          close();
          decoder = new VideoDecoder({ output: draw, error: fail });
          decoder.configure({ codec: wanted, optimizeForLatency: true });
          codec = wanted;
        }
        // Units before the first key frame can't be decoded.
        if (!decoder || decoder.state !== 'configured') return;
        decoder.decode(new EncodedVideoChunk({ type: key ? 'key' : 'delta', timestamp: count++ * 33_333, data: unit }));
      } catch (e) {
        fail(e as Error);
      }
    },
    close() {
      failed = true;
      close();
    },
  };
}
