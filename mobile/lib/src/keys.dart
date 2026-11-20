import 'package:flutter/services.dart';

/// Physical keys of a hardware keyboard (Bluetooth, USB-C) → the W3C `code` names the
/// host understands (`rdesk_proto::Key`).
final Map<PhysicalKeyboardKey, String> physicalCodes = {
  for (var i = 0; i < 26; i++) _letters[i]: 'Key${String.fromCharCode(65 + i)}',
  for (var i = 0; i < 10; i++) _digits[i]: 'Digit$i',
  for (var i = 0; i < 12; i++) _fkeys[i]: 'F${i + 1}',
  PhysicalKeyboardKey.enter: 'Enter',
  PhysicalKeyboardKey.escape: 'Escape',
  PhysicalKeyboardKey.backspace: 'Backspace',
  PhysicalKeyboardKey.tab: 'Tab',
  PhysicalKeyboardKey.space: 'Space',
  PhysicalKeyboardKey.minus: 'Minus',
  PhysicalKeyboardKey.equal: 'Equal',
  PhysicalKeyboardKey.bracketLeft: 'BracketLeft',
  PhysicalKeyboardKey.bracketRight: 'BracketRight',
  PhysicalKeyboardKey.backslash: 'Backslash',
  PhysicalKeyboardKey.semicolon: 'Semicolon',
  PhysicalKeyboardKey.quote: 'Quote',
  PhysicalKeyboardKey.backquote: 'Backquote',
  PhysicalKeyboardKey.comma: 'Comma',
  PhysicalKeyboardKey.period: 'Period',
  PhysicalKeyboardKey.slash: 'Slash',
  PhysicalKeyboardKey.intlBackslash: 'IntlBackslash',
  PhysicalKeyboardKey.intlRo: 'IntlRo',
  PhysicalKeyboardKey.capsLock: 'CapsLock',
  PhysicalKeyboardKey.printScreen: 'PrintScreen',
  PhysicalKeyboardKey.scrollLock: 'ScrollLock',
  PhysicalKeyboardKey.pause: 'Pause',
  PhysicalKeyboardKey.insert: 'Insert',
  PhysicalKeyboardKey.home: 'Home',
  PhysicalKeyboardKey.pageUp: 'PageUp',
  PhysicalKeyboardKey.delete: 'Delete',
  PhysicalKeyboardKey.end: 'End',
  PhysicalKeyboardKey.pageDown: 'PageDown',
  PhysicalKeyboardKey.arrowRight: 'ArrowRight',
  PhysicalKeyboardKey.arrowLeft: 'ArrowLeft',
  PhysicalKeyboardKey.arrowDown: 'ArrowDown',
  PhysicalKeyboardKey.arrowUp: 'ArrowUp',
  PhysicalKeyboardKey.numLock: 'NumLock',
  PhysicalKeyboardKey.numpadDivide: 'NumpadDivide',
  PhysicalKeyboardKey.numpadMultiply: 'NumpadMultiply',
  PhysicalKeyboardKey.numpadSubtract: 'NumpadSubtract',
  PhysicalKeyboardKey.numpadAdd: 'NumpadAdd',
  PhysicalKeyboardKey.numpadEnter: 'NumpadEnter',
  PhysicalKeyboardKey.numpadDecimal: 'NumpadDecimal',
  PhysicalKeyboardKey.numpad0: 'Numpad0',
  PhysicalKeyboardKey.numpad1: 'Numpad1',
  PhysicalKeyboardKey.numpad2: 'Numpad2',
  PhysicalKeyboardKey.numpad3: 'Numpad3',
  PhysicalKeyboardKey.numpad4: 'Numpad4',
  PhysicalKeyboardKey.numpad5: 'Numpad5',
  PhysicalKeyboardKey.numpad6: 'Numpad6',
  PhysicalKeyboardKey.numpad7: 'Numpad7',
  PhysicalKeyboardKey.numpad8: 'Numpad8',
  PhysicalKeyboardKey.numpad9: 'Numpad9',
  PhysicalKeyboardKey.contextMenu: 'ContextMenu',
  PhysicalKeyboardKey.controlLeft: 'ControlLeft',
  PhysicalKeyboardKey.shiftLeft: 'ShiftLeft',
  PhysicalKeyboardKey.altLeft: 'AltLeft',
  PhysicalKeyboardKey.metaLeft: 'MetaLeft',
  PhysicalKeyboardKey.controlRight: 'ControlRight',
  PhysicalKeyboardKey.shiftRight: 'ShiftRight',
  PhysicalKeyboardKey.altRight: 'AltRight',
  PhysicalKeyboardKey.metaRight: 'MetaRight',
};

const _letters = [
  PhysicalKeyboardKey.keyA, PhysicalKeyboardKey.keyB, PhysicalKeyboardKey.keyC, PhysicalKeyboardKey.keyD,
  PhysicalKeyboardKey.keyE, PhysicalKeyboardKey.keyF, PhysicalKeyboardKey.keyG, PhysicalKeyboardKey.keyH,
  PhysicalKeyboardKey.keyI, PhysicalKeyboardKey.keyJ, PhysicalKeyboardKey.keyK, PhysicalKeyboardKey.keyL,
  PhysicalKeyboardKey.keyM, PhysicalKeyboardKey.keyN, PhysicalKeyboardKey.keyO, PhysicalKeyboardKey.keyP,
  PhysicalKeyboardKey.keyQ, PhysicalKeyboardKey.keyR, PhysicalKeyboardKey.keyS, PhysicalKeyboardKey.keyT,
  PhysicalKeyboardKey.keyU, PhysicalKeyboardKey.keyV, PhysicalKeyboardKey.keyW, PhysicalKeyboardKey.keyX,
  PhysicalKeyboardKey.keyY, PhysicalKeyboardKey.keyZ,
];

const _digits = [
  PhysicalKeyboardKey.digit0, PhysicalKeyboardKey.digit1, PhysicalKeyboardKey.digit2, PhysicalKeyboardKey.digit3,
  PhysicalKeyboardKey.digit4, PhysicalKeyboardKey.digit5, PhysicalKeyboardKey.digit6, PhysicalKeyboardKey.digit7,
  PhysicalKeyboardKey.digit8, PhysicalKeyboardKey.digit9,
];

const _fkeys = [
  PhysicalKeyboardKey.f1, PhysicalKeyboardKey.f2, PhysicalKeyboardKey.f3, PhysicalKeyboardKey.f4,
  PhysicalKeyboardKey.f5, PhysicalKeyboardKey.f6, PhysicalKeyboardKey.f7, PhysicalKeyboardKey.f8,
  PhysicalKeyboardKey.f9, PhysicalKeyboardKey.f10, PhysicalKeyboardKey.f11, PhysicalKeyboardKey.f12,
];

/// The key that types `ch` on a US layout, for shortcuts typed on the phone keyboard while
/// a modifier is latched (Ctrl + "c" → `KeyC`). Null when there is none.
String? codeForChar(String ch) {
  if (ch.length != 1) return null;
  final c = ch.toUpperCase().codeUnitAt(0);
  if (c >= 65 && c <= 90) return 'Key${String.fromCharCode(c)}';
  if (c >= 48 && c <= 57) return 'Digit${String.fromCharCode(c)}';
  return const {
    ' ': 'Space', '-': 'Minus', '=': 'Equal', '[': 'BracketLeft', ']': 'BracketRight', '\\': 'Backslash',
    ';': 'Semicolon', "'": 'Quote', '`': 'Backquote', ',': 'Comma', '.': 'Period', '/': 'Slash', '\n': 'Enter',
    '\t': 'Tab',
  }[ch];
}

/// What a terminal key sends (xterm), for the terminal's key bar.
String? terminalSequence(String code, {bool ctrl = false, bool alt = false}) {
  const seq = {
    'Escape': '\x1b', 'Tab': '\t', 'Enter': '\r', 'Backspace': '\x7f',
    'ArrowUp': '\x1b[A', 'ArrowDown': '\x1b[B', 'ArrowRight': '\x1b[C', 'ArrowLeft': '\x1b[D',
    'Home': '\x1b[H', 'End': '\x1b[F', 'PageUp': '\x1b[5~', 'PageDown': '\x1b[6~', 'Delete': '\x1b[3~',
    'Insert': '\x1b[2~',
  };
  final s = seq[code];
  if (s == null) return null;
  return alt ? '\x1b$s' : s;
}
