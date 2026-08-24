/// Where the caret is, in pixels, inside a textarea.
///
/// A textarea exposes the caret as an **offset into a string** and nothing
/// else — there is no API for its position on screen. The way to get one is
/// to build a hidden element that lays the same text out under the same
/// rules, drop a marker where the caret is, and read the marker's position.
/// Every property below changes where a glyph lands, so all of them have to
/// be copied or the mirror wraps somewhere the real field does not.
const MIRRORED = [
  'box-sizing',
  'width',
  'padding-top',
  'padding-right',
  'padding-bottom',
  'padding-left',
  'border-top-width',
  'border-right-width',
  'border-bottom-width',
  'border-left-width',
  'font-family',
  'font-size',
  'font-weight',
  'font-style',
  'letter-spacing',
  'line-height',
  'text-transform',
  'word-spacing',
  'text-indent',
  'tab-size',
] as const;

export type CaretPoint = {
  /// Distance from the field's top edge to the top of the caret's line,
  /// already corrected for how far the field is scrolled.
  top: number;
  left: number;
  /// The height of one line, so a caller can clear the caret's own line
  /// rather than covering it.
  lineHeight: number;
  /// The field's own height, so a popup can be anchored to its bottom edge
  /// and open upward — which is the only direction available to a composer
  /// pinned to the bottom of the page.
  fieldHeight: number;
};

export function caretPoint(box: HTMLTextAreaElement, caret: number): CaretPoint {
  const style = window.getComputedStyle(box);
  const mirror = document.createElement('div');
  for (const property of MIRRORED) {
    mirror.style.setProperty(property, style.getPropertyValue(property));
  }
  // Off-screen rather than `display: none`: a box that is not laid out has
  // no geometry to read.
  mirror.style.position = 'absolute';
  mirror.style.top = '0';
  mirror.style.left = '-9999px';
  mirror.style.visibility = 'hidden';
  mirror.style.whiteSpace = 'pre-wrap';
  mirror.style.overflowWrap = 'break-word';

  mirror.textContent = box.value.slice(0, caret);
  const marker = document.createElement('span');
  // A marker with nothing in it has no height on its own line, so it is fed
  // the rest of the text — and a full stop when there is none, for a caret
  // sitting at the very end.
  marker.textContent = box.value.slice(caret) === '' ? '.' : box.value.slice(caret);
  mirror.appendChild(marker);

  document.body.appendChild(mirror);
  const top = marker.offsetTop;
  const left = marker.offsetLeft;
  const lineHeight = Number.parseFloat(style.lineHeight);
  document.body.removeChild(mirror);

  return {
    top: top - box.scrollTop,
    left,
    lineHeight: Number.isFinite(lineHeight) ? lineHeight : 20,
    fieldHeight: box.clientHeight,
  };
}
