/**
 * Parse a trim-setting text field into a number inside [min, max].
 * Empty or non-numeric text yields `fallback`. Zero is a real value and is
 * clamped like any other (it must not fall back to the default).
 *
 * @param {string} text
 * @param {number} min
 * @param {number} max
 * @param {number} fallback
 * @returns {number}
 */
export function parseTrimInput(text, min, max, fallback) {
  const v = parseFloat(text);
  if (Number.isNaN(v)) return fallback;
  return Math.min(max, Math.max(min, v));
}

/**
 * Regions the conversion would remove, given the backend's trim decision.
 * Only a side the backend reports as actually cut is drawn, so a real cut is
 * never hidden and a sliver from differing duration measurements never appears.
 * The waveform's duration is the right edge; the backend's is the fallback.
 *
 * @param {{outcome: string, keepStart: number, keepEnd: number,
 *          cutHead: boolean, cutTail: boolean, duration: number}} preview
 * @param {number} totalDuration
 * @returns {Array<[number, number]>}
 */
export function removedRegions(
  { outcome, keepStart, keepEnd, cutHead, cutTail, duration },
  totalDuration,
) {
  if (outcome !== 'trim') return [];
  const end = totalDuration > 0 ? totalDuration : duration;
  const regions = [];
  if (cutHead) regions.push([0, keepStart]);
  if (cutTail) regions.push([keepEnd, end]);
  return regions;
}

/**
 * Tracks which asynchronous request is the newest. A result or error from any
 * older request must be ignored, including a rejection caused by being superseded.
 *
 * @returns {{begin: () => number, isLatest: (id: number) => boolean}}
 */
export function createLatestTracker() {
  let latest = 0;
  return {
    begin: () => ++latest,
    isLatest: (id) => id === latest,
  };
}
