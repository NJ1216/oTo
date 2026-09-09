/**
 * RMSサンプル列から無音領域を検出する純粋関数。
 *
 * @param {number[]} rmsValues  - 波形の RMS 値の配列
 * @param {number}   db         - 無音閾値 (dBFS, 例: -80)
 * @param {number}   minDurationSecs - 最小無音長 (秒)
 * @param {number}   totalDuration   - 音声の総長 (秒)
 * @returns {Array<[number, number]>} - 無音領域の [開始, 終了] ペア配列（先頭/末尾のみ）
 */
export function detectSilence(rmsValues, db, minDurationSecs, totalDuration) {
  const dbLinear = Math.pow(10, db / 20);
  const n = rmsValues.length;
  if (n === 0) return [];

  const sampleDur = totalDuration / n;
  const allRegions = [];
  let inSilence = false;
  let silenceStart = 0;

  for (let i = 0; i < n; i++) {
    const isQuiet = rmsValues[i] < dbLinear;

    if (isQuiet && !inSilence) {
      inSilence = true;
      silenceStart = i * sampleDur;
    } else if (!isQuiet && inSilence) {
      const silenceEnd = i * sampleDur;
      if (silenceEnd - silenceStart >= minDurationSecs) {
        allRegions.push([silenceStart, silenceEnd]);
      }
      inSilence = false;
    }
  }

  if (inSilence) {
    const silenceEnd = totalDuration;
    if (silenceEnd - silenceStart >= minDurationSecs) {
      allRegions.push([silenceStart, silenceEnd]);
    }
  }

  if (allRegions.length === 0) return [];

  const tolerance = 0.05; // 50ms — Rust 側の detect_boundary_silence と同値
  const result = [];

  // 先頭無音: 最初の領域が先頭付近から始まる場合
  if (allRegions[0][0] <= tolerance) {
    result.push(allRegions[0]);
  }

  // 末尾無音: 最後の領域が末尾付近で終わる場合（同一領域の重複追加は防ぐ）
  const last = allRegions[allRegions.length - 1];
  if (Math.abs(totalDuration - last[1]) <= tolerance) {
    if (result.length === 0 || last !== result[0]) {
      result.push(last);
    }
  }

  return result;
}
