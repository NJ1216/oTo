export const FORMAT_THEME = {
  mp3:  { color: '#FF9800', colorDark: '#4A2800', colorSecondary: '#E68900' },
  aac:  { color: '#2196F3', colorDark: '#0A2A4A', colorSecondary: '#1565C0' },
  wav:  { color: '#4CAF50', colorDark: '#1A3A1A', colorSecondary: '#388E3C' },
  flac: { color: '#9C27B0', colorDark: '#2A0A3A', colorSecondary: '#7B1FA2' },
  alac: { color: '#FFC107', colorDark: '#3D2900', colorSecondary: '#FF8F00' },
  ogg:  { color: '#00BCD4', colorDark: '#003038', colorSecondary: '#0097A7' },
  opus: { color: '#E91E63', colorDark: '#3D0020', colorSecondary: '#C2185B' },
  aiff: { color: '#5C6BC0', colorDark: '#0D1240', colorSecondary: '#3949AB' },
};

// Base template colors (MP3/ENCODE)
const BASE_COLOR       = '#FF9800';
const BASE_COLOR_SEC   = '#E68900';
const BASE_COLOR_DARK  = '#4A2800';
const BASE_FORMAT_TEXT = 'MP3';
const BASE_MODE_ENCODE = 'ENCODE';
const BASE_MODE_DECODE = 'DECODE';

const ARC_LENGTH = 251.327; // π × r(80) = half-circle circumference

let svgCache = {};
let currentState = 'standby';
let currentFormat = 'mp3';
let currentMode = 'encode';
let container = null;

// Preload all SVG strings
export async function initSVGController(containerEl, format, mode) {
  container = containerEl;
  currentFormat = format;
  currentMode = mode;

  const names = ['background_standby', 'background_hover', 'background_processing'];
  await Promise.all(
    names.map(async (name) => {
      try {
        const r = await fetch(`svgs/${name}.svg`);
        svgCache[name] = await r.text();
      } catch (e) {
        console.error(`Failed to load ${name}.svg`, e);
        svgCache[name] = '';
      }
    })
  );

  setState('standby');
}

// Apply format/mode/color substitutions to SVG string
function applyTheme(svgString, format, mode) {
  const theme = FORMAT_THEME[format] || FORMAT_THEME.mp3;
  const modeText = mode === 'decode' ? BASE_MODE_DECODE : BASE_MODE_ENCODE;

  return svgString
    .replace(new RegExp(BASE_COLOR.replace('#', '\\#'), 'g'), theme.color)
    .replace(new RegExp(BASE_COLOR_SEC.replace('#', '\\#'), 'g'), theme.colorSecondary)
    .replace(new RegExp(BASE_COLOR_DARK.replace('#', '\\#'), 'g'), theme.colorDark)
    .replace(/\bMP3\b/g, format.toUpperCase())
    .replace(/\bENCODE\b/g, modeText)
    .replace(/\bDECODE\b/g, modeText);
}

function getSVGName(state) {
  return {
    standby:    'background_standby',
    hover:      'background_hover',
    processing: 'background_processing',
  }[state] || 'background_standby';
}

export function setState(state) {
  if (!container) return;
  currentState = state;

  const svgName = getSVGName(state);
  const raw = svgCache[svgName] || '';
  const themed = applyTheme(raw, currentFormat, currentMode);
  container.innerHTML = themed;

  if (state === 'processing') {
    prepareProgressArc();
  }
}

export function setFormat(format) {
  currentFormat = format;
  setState(currentState);
}

export function setMode(mode) {
  currentMode = mode;
  setState(currentState);
}

// Set up the progress arc for JS control (remove SMIL animation)
function prepareProgressArc() {
  const arc = container.querySelector('path[stroke-dasharray]');
  if (!arc) return;
  arc.id = 'progress-arc';

  // Remove SMIL <animate> so JS takes over
  arc.querySelectorAll('animate').forEach((a) => a.remove());

  // Enable CSS transition for smooth animation
  arc.style.transition = 'stroke-dashoffset 0.5s linear';
  arc.setAttribute('stroke-dasharray', `${ARC_LENGTH} ${ARC_LENGTH}`);
  arc.setAttribute('stroke-dashoffset', ARC_LENGTH);
}

// Update progress arc (0–100)
export function setProgress(percent) {
  const arc = document.getElementById('progress-arc');
  if (!arc) return;
  const offset = ARC_LENGTH * (1 - Math.min(percent, 100) / 100);
  arc.style.strokeDashoffset = offset;
  const percentEl = document.getElementById('processing-percent');
  if (percentEl) percentEl.textContent = Math.round(percent) + '%';
}
