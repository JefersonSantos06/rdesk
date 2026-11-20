import 'dart:async';
import 'dart:convert';

import 'package:material_ui/material_ui.dart';
import 'package:flutter/services.dart';
import 'package:xterm/xterm.dart';

import 'i18n.dart';
import 'keys.dart';
import 'rust/api/session.dart';
import 'session_hub.dart';

/// A shell on the computer, with a key bar for what the phone keyboard lacks.
class TerminalPage extends StatefulWidget {
  const TerminalPage({super.key, required this.hub});
  final SessionHub hub;

  @override
  State<TerminalPage> createState() => _TerminalPageState();
}

class _TerminalPageState extends State<TerminalPage> {
  final _terminal = Terminal(maxLines: 10000);
  final _controller = TerminalController();
  StreamSubscription<SessionEvent>? _sub;
  int? _id;
  bool _asked = false;
  bool _ctrl = false;
  bool _alt = false;
  String _title = '';
  String? _ended;

  @override
  void initState() {
    super.initState();
    _sub = widget.hub.events.listen(_onEvent);
    widget.hub.addListener(_maybeOpen);
    _terminal.onOutput = _input;
    _terminal.onResize = (w, h, pw, ph) {
      final id = _id;
      if (id != null) widget.hub.session.terminalResize(id: id, cols: w, rows: h);
    };
    _maybeOpen();
  }

  @override
  void dispose() {
    final id = _id;
    if (id != null) widget.hub.session.terminalClose(id: id);
    _sub?.cancel();
    widget.hub.removeListener(_maybeOpen);
    super.dispose();
  }

  void _maybeOpen() {
    if (_asked || !widget.hub.connected) return;
    _asked = true;
    widget.hub.session.openTerminal(cols: _terminal.viewWidth, rows: _terminal.viewHeight, admin: false);
  }

  final _utf8 = _Utf8Stream();

  void _onEvent(SessionEvent e) {
    if (!mounted) return;
    switch (e.kind) {
      case EventKind.terminalOpened:
        if (_id == null || _id == e.id) {
          setState(() {
            _id = e.id;
            _title = e.text;
          });
          widget.hub.session.terminalResize(id: e.id, cols: _terminal.viewWidth, rows: _terminal.viewHeight);
        }
      case EventKind.terminalOutput:
        if (e.id == _id) _terminal.write(_utf8.decode(e.bytes));
      case EventKind.terminalExited:
        if (_id == null || e.id == _id) setState(() => _ended = e.text.isEmpty ? t('The shell ended.', 'O shell terminou.') : e.text);
      default:
    }
  }

  void _input(String data) {
    final id = _id;
    if (id == null) return;
    var s = data;
    if (_ctrl && s.length == 1) {
      final c = s.toUpperCase().codeUnitAt(0);
      if (c >= 64 && c <= 95) s = String.fromCharCode(c - 64);
      setState(() => _ctrl = false);
    }
    if (_alt) {
      s = '\x1b$s';
      setState(() => _alt = false);
    }
    widget.hub.session.terminalInput(id: id, data: utf8.encode(s));
  }

  void _key(String code) {
    final s = terminalSequence(code, alt: _alt);
    if (s == null) return;
    final id = _id;
    if (id != null) widget.hub.session.terminalInput(id: id, data: utf8.encode(s));
    if (_alt) setState(() => _alt = false);
  }

  Future<void> _paste() async {
    final d = await Clipboard.getData(Clipboard.kTextPlain);
    final text = d?.text;
    if (text != null && text.isNotEmpty) _terminal.paste(text);
  }

  void _copy() {
    final sel = _controller.selection;
    if (sel == null) return;
    Clipboard.setData(ClipboardData(text: _terminal.buffer.getText(sel)));
    _controller.clearSelection();
    ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text(t('Copied.', 'Copiado.'))));
  }

  @override
  Widget build(BuildContext context) {
    final hub = widget.hub;
    return Scaffold(
      backgroundColor: const Color(0xFF101010),
      appBar: AppBar(
        title: Text(_title.isEmpty ? t('Terminal', 'Terminal') : _title, overflow: TextOverflow.ellipsis),
        actions: [
          IconButton(tooltip: t('Copy', 'Copiar'), icon: const Icon(Icons.copy), onPressed: _copy),
          IconButton(tooltip: t('Paste', 'Colar'), icon: const Icon(Icons.paste), onPressed: _paste),
        ],
      ),
      body: SafeArea(
        child: Column(children: [
          if (!hub.connected || _ended != null)
            MaterialBanner(
              content: Text(_ended ?? hub.fatal ?? (hub.status.isEmpty ? t('Connecting…', 'Conectando…') : hub.status)),
              actions: [
                if (_ended != null)
                  TextButton(
                    onPressed: () {
                      setState(() {
                        _ended = null;
                        _id = null;
                        _asked = false;
                      });
                      _maybeOpen();
                    },
                    child: Text(t('Open again', 'Abrir de novo')),
                  )
                else
                  TextButton(onPressed: hub.session.reconnect, child: Text(t('Try now', 'Tentar agora'))),
              ],
            ),
          Expanded(
            child: TerminalView(
              _terminal,
              controller: _controller,
              autofocus: true,
              backgroundOpacity: 1,
              // No word suggestions: the keyboard must hand over each key as typed (a
              // keyboard composing words sends them again and again).
              keyboardType: TextInputType.visiblePassword,
              textStyle: const TerminalStyle(fontSize: 13),
            ),
          ),
          Material(
            color: const Color(0xFF1D2128),
            child: SizedBox(
              height: 48,
              child: ListView(scrollDirection: Axis.horizontal, padding: const EdgeInsets.all(6), children: [
                _chip('Ctrl', _ctrl, () => setState(() => _ctrl = !_ctrl)),
                _chip('Alt', _alt, () => setState(() => _alt = !_alt)),
                for (final (code, label) in const [
                  ('Escape', 'Esc'),
                  ('Tab', 'Tab'),
                  ('ArrowUp', '↑'),
                  ('ArrowDown', '↓'),
                  ('ArrowLeft', '←'),
                  ('ArrowRight', '→'),
                  ('Home', 'Home'),
                  ('End', 'End'),
                  ('PageUp', 'PgUp'),
                  ('PageDown', 'PgDn'),
                ])
                  Padding(
                    padding: const EdgeInsets.symmetric(horizontal: 2),
                    child: ActionChip(label: Text(label), onPressed: () => _key(code)),
                  ),
              ]),
            ),
          ),
        ]),
      ),
    );
  }

  Widget _chip(String label, bool on, VoidCallback onTap) => Padding(
        padding: const EdgeInsets.symmetric(horizontal: 2),
        child: FilterChip(label: Text(label), selected: on, onSelected: (_) => onTap()),
      );
}

/// Decodes a byte stream as UTF-8 across chunk boundaries (a character may be split).
class _Utf8Stream {
  final List<int> _rest = [];

  String decode(Uint8List chunk) {
    final all = [..._rest, ...chunk];
    _rest.clear();
    // Hold back an incomplete sequence at the end.
    var cut = all.length;
    for (var i = all.length - 1; i >= 0 && i >= all.length - 4; i--) {
      final b = all[i];
      if (b & 0xC0 == 0x80) continue; // continuation byte
      final need = b >= 0xF0 ? 4 : b >= 0xE0 ? 3 : b >= 0xC0 ? 2 : 1;
      if (all.length - i < need) cut = i;
      break;
    }
    _rest.addAll(all.sublist(cut));
    return utf8.decode(all.sublist(0, cut), allowMalformed: true);
  }
}
