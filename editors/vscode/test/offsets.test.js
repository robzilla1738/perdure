// Plain-node test for the byte-offset → position mapper. Run with:
//   node editors/vscode/test/offsets.test.js
// Pins the one subtle piece of the extension: UTF-8 byte offsets (what
// `perdure check --json` emits) vs UTF-16 character columns (what VS Code
// positions use), which diverge on any non-ASCII file.

"use strict";

const assert = require("node:assert");
const { byteToPositionMapper } = require("../offsets");

// ASCII: bytes == characters.
{
  const toPos = byteToPositionMapper("fn f() {\n  return 1\n}\n");
  assert.deepStrictEqual(toPos(0), { line: 0, character: 0 });
  assert.deepStrictEqual(toPos(3), { line: 0, character: 3 });
  assert.deepStrictEqual(toPos(9), { line: 1, character: 0 });
  assert.deepStrictEqual(toPos(11), { line: 1, character: 2 }); // 'r' of return
  assert.deepStrictEqual(toPos(20), { line: 2, character: 0 }); // '}'
}

// Two-byte characters: 'é' is 2 UTF-8 bytes, 1 UTF-16 unit.
{
  //        bytes:  é=2, so "// résumé\n" = 2+1+1+1+2+1+1+1+2+1 ... compute:
  const text = "// résumé\nlet x = 1\n";
  const toPos = byteToPositionMapper(text);
  // "// r" -> byte 3 is 'r' at character 3.
  assert.deepStrictEqual(toPos(3), { line: 0, character: 3 });
  // 'é' at character 4 occupies bytes 4-5; byte 6 is 's' at character 5.
  assert.deepStrictEqual(toPos(6), { line: 0, character: 5 });
  // A byte INSIDE the two-byte 'é' snaps to the 'é' itself.
  assert.deepStrictEqual(toPos(5), { line: 0, character: 4 });
  // "// résumé" is 11 bytes (2 extra for the two é); newline at byte 11.
  assert.deepStrictEqual(toPos(12), { line: 1, character: 0 }); // 'l' of let
}

// Three-byte characters: '→' is 3 UTF-8 bytes, 1 UTF-16 unit.
{
  const text = "// a → b\nx";
  const toPos = byteToPositionMapper(text);
  // bytes: '/'0 '/'1 ' '2 'a'3 ' '4 '→'5-7 ' '8 'b'9 '\n'10 'x'11
  assert.deepStrictEqual(toPos(5), { line: 0, character: 5 });
  assert.deepStrictEqual(toPos(8), { line: 0, character: 6 });
  assert.deepStrictEqual(toPos(9), { line: 0, character: 7 });
  assert.deepStrictEqual(toPos(11), { line: 1, character: 0 });
}

// Astral plane: '🙂' is 4 UTF-8 bytes and TWO UTF-16 units.
{
  const text = "// 🙂!\nok";
  const toPos = byteToPositionMapper(text);
  // bytes: '/'0 '/'1 ' '2 '🙂'3-6 '!'7 '\n'8 'o'9
  assert.deepStrictEqual(toPos(3), { line: 0, character: 3 });
  assert.deepStrictEqual(toPos(7), { line: 0, character: 5 }); // after 2-unit emoji
  assert.deepStrictEqual(toPos(9), { line: 1, character: 0 });
}

// Offsets at/past EOF clamp to the end position.
{
  const toPos = byteToPositionMapper("ab\n");
  assert.deepStrictEqual(toPos(3), { line: 1, character: 0 });
  assert.deepStrictEqual(toPos(99), { line: 1, character: 0 });
}

// Empty file.
{
  const toPos = byteToPositionMapper("");
  assert.deepStrictEqual(toPos(0), { line: 0, character: 0 });
}

console.log("offsets.test.js: all assertions passed");
