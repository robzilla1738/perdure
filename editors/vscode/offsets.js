// Byte-offset → editor-position mapping, factored out of extension.js so it
// can be tested with plain `node` (no VS Code host).
//
// `perdure check --json` reports spans as BYTE offsets into the UTF-8 source.
// VS Code positions are (line, character) where character counts UTF-16 code
// units. The two diverge on any non-ASCII file, so the mapping walks code
// points, tracking the UTF-8 byte length and UTF-16 unit length of each.

"use strict";

/**
 * Build a mapper from UTF-8 byte offsets into `text` to {line, character}
 * positions (character in UTF-16 code units, the editor's unit).
 *
 * Offsets inside a multi-byte character snap to that character's position;
 * offsets at or past the end of the text map to the end position.
 */
function byteToPositionMapper(text) {
  // One entry per code point: its starting byte offset, line, and character.
  // Plus a sentinel for end-of-text.
  const bytes = [];
  const lines = [];
  const chars = [];
  let byte = 0;
  let line = 0;
  let character = 0;
  for (const cp of text) {
    bytes.push(byte);
    lines.push(line);
    chars.push(character);
    byte += utf8Len(cp.codePointAt(0));
    if (cp === "\n") {
      line += 1;
      character = 0;
    } else {
      character += cp.length; // UTF-16 units (2 for astral code points)
    }
  }
  bytes.push(byte);
  lines.push(line);
  chars.push(character);

  return function (byteOffset) {
    // Binary search the greatest index with bytes[i] <= byteOffset.
    let lo = 0;
    let hi = bytes.length - 1;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (bytes[mid] <= byteOffset) {
        lo = mid;
      } else {
        hi = mid - 1;
      }
    }
    return { line: lines[lo], character: chars[lo] };
  };
}

function utf8Len(codePoint) {
  if (codePoint < 0x80) return 1;
  if (codePoint < 0x800) return 2;
  if (codePoint < 0x10000) return 3;
  return 4;
}

module.exports = { byteToPositionMapper };
