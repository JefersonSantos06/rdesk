import 'dart:async';
import 'dart:math' as math;
import 'dart:ui' as ui;

import 'package:flutter/gestures.dart';
import 'package:material_ui/material_ui.dart';
import 'package:flutter/services.dart';
import 'package:wakelock_plus/wakelock_plus.dart';

import 'computers_page.dart' show Open;
import 'device_key.dart';
import 'files_page.dart';
import 'i18n.dart';
import 'keys.dart';
import 'rust/api/session.dart';
import 'session_hub.dart';
import 'terminal_page.dart';

/// How touches drive the remote pointer.
enum TouchMode {
  /// Like a laptop's touchpad: drag moves the pointer, tap clicks where it is.
  touchpad,

  /// Touch where you want: tap clicks there, drag drags there.
  direct,
}

/// A remote session: the computer's screen, with touch, the phone keyboard and a key bar.
class SessionPage extends StatefulWidget {
  const SessionPage({super.key, required this.target, this.open = Open.screen});
  final Target target;
  final Open open;

  @override
  State<SessionPage> createState() => _SessionPageState();
}

class _SessionPageState extends State<SessionPage> with WidgetsBindingObserver {
  late final SessionHub _hub;
  StreamSubscription<SessionEvent>? _sub;

  // ---- picture ----
  ui.Image? _image;
  bool _decoding = false;
  bool _framePending = false;
  Size _view = Size.zero;
  double _zoom = 1;
  Offset _pan = Offset.zero;

  // ---- pointer ----
  TouchMode _mode = TouchMode.touchpad;
  Offset _cursor = const Offset(0.5, 0.5);
  bool _cursorVisible = true;
  final Map<int, Offset> _pointers = {};
  Offset? _downPos;
  DateTime _downAt = DateTime.now();
  bool _moved = false;
  bool _holding = false;
  bool _longPressed = false;
  int _maxPointers = 0;
  Timer? _longPress;
  String? _twoFinger; // null (undecided), 'zoom' or 'scroll'
  double _startDist = 1;
  Offset _startCentroid = Offset.zero;
  Offset _lastCentroid = Offset.zero;
  double _startZoom = 1;
  Offset _startPan = Offset.zero;

  // ---- keyboard ----
  static const _sentinel = '  ';
  final _kb = TextEditingController(text: _sentinel);
  final _kbFocus = FocusNode();
  final _hwFocus = FocusNode();
  bool _keysBar = false;
  final Set<String> _latched = {};

  // ---- chrome ----
  bool _toolbar = true;
  bool _passwordOpen = false;

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addObserver(this);
    final view = WidgetsBinding.instance.platformDispatcher.views.first;
    final long = (math.max(view.physicalSize.width, view.physicalSize.height)).round();
    _hub = SessionHub(widget.target, video: widget.open == Open.screen, maxWidth: long, maxHeight: long);
    _hub.addListener(_changed);
    _kb.addListener(_onKeyboardText);
    DeviceKey.publicKey().then((pk) async {
      if (pk != null) {
        try {
          await _hub.session.useDeviceKey(
            publicKey: pk,
            sign: (data) => DeviceKey.sign(data, t('Sign in to ${widget.target.name}', 'Entrar em ${widget.target.name}')),
          );
        } catch (_) {}
      }
    }).whenComplete(() {
      _sub = _hub.events.listen(_onEvent);
      _hub.start();
    });
    WakelockPlus.enable();
    SystemChrome.setEnabledSystemUIMode(SystemUiMode.immersiveSticky);
    if (widget.open != Open.screen) {
      WidgetsBinding.instance.addPostFrameCallback((_) => _openPanel(widget.open));
    }
  }

  @override
  void dispose() {
    WidgetsBinding.instance.removeObserver(this);
    _longPress?.cancel();
    _sub?.cancel();
    _hub.removeListener(_changed);
    _hub.dispose();
    _image?.dispose();
    _kb.dispose();
    _kbFocus.dispose();
    _hwFocus.dispose();
    WakelockPlus.disable();
    SystemChrome.setEnabledSystemUIMode(SystemUiMode.edgeToEdge);
    super.dispose();
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    if (state == AppLifecycleState.paused || state == AppLifecycleState.inactive) {
      // Never leave a key or button held on the computer.
      _hub.session.releaseAll();
      _holding = false;
    } else if (state == AppLifecycleState.resumed) {
      // The phone may have dropped the connection while away: try at once.
      if (!_hub.connected) _hub.session.reconnect();
    }
  }

  void _changed() {
    if (mounted) setState(() {});
  }

  void _onEvent(SessionEvent e) {
    switch (e.kind) {
      case EventKind.frame:
        _onFrame();
      case EventKind.cursor:
        // While the finger moves the pointer, the phone's idea of where it is wins.
        // An invisible pointer (another monitor, or unknown) keeps the phone's own idea.
        if (e.visible && (!_moved || _pointers.isEmpty)) _cursor = Offset(e.x / 65535, e.y / 65535);
        _cursorVisible = e.visible;
        if (mounted) setState(() {});
      case EventKind.clipboard:
        Clipboard.setData(ClipboardData(text: e.text));
      case EventKind.notice:
        _snack(e.text);
      case EventKind.passwordNeeded:
        _askPassword(e);
      case EventKind.transferDone:
        _snack(e.ok ? t('Saved: ${e.text}', 'Salvo: ${e.text}') : e.text);
      default:
    }
  }

  void _snack(String text) {
    if (!mounted) return;
    ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text(text), duration: const Duration(seconds: 3)));
  }

  // ---- picture ----

  void _onFrame() {
    if (_decoding) {
      _framePending = true;
      return;
    }
    _decode();
  }

  Future<void> _decode() async {
    _decoding = true;
    final f = _hub.session.takeFrame();
    if (f != null && f.width > 0 && f.height > 0) {
      final c = Completer<ui.Image>();
      ui.decodeImageFromPixels(f.rgba, f.width, f.height, ui.PixelFormat.rgba8888, c.complete);
      final img = await c.future;
      if (!mounted) {
        img.dispose();
        return;
      }
      final old = _image;
      setState(() => _image = img);
      if (old != null) WidgetsBinding.instance.addPostFrameCallback((_) => old.dispose());
    }
    _decoding = false;
    if (_framePending) {
      _framePending = false;
      _decode();
    }
  }

  /// Where the picture sits in the view at zoom 1.
  Rect get _dst {
    final img = _image;
    if (img == null || _view.isEmpty) return Offset.zero & _view;
    final fit = applyBoxFit(BoxFit.contain, Size(img.width.toDouble(), img.height.toDouble()), _view);
    return Alignment.center.inscribe(fit.destination, Offset.zero & _view);
  }

  /// A point on the screen → 0..1 across the remote monitor.
  Offset _toRemote(Offset p) {
    final q = (p - _pan) / _zoom;
    final d = _dst;
    return Offset(((q.dx - d.left) / d.width).clamp(0.0, 1.0), ((q.dy - d.top) / d.height).clamp(0.0, 1.0));
  }

  Offset _toScreen(Offset remote) {
    final d = _dst;
    return Offset(d.left + remote.dx * d.width, d.top + remote.dy * d.height) * _zoom + _pan;
  }

  void _clampPan() {
    final maxX = _view.width * (_zoom - 1);
    final maxY = _view.height * (_zoom - 1);
    _pan = Offset(_pan.dx.clamp(-maxX, 0.0), _pan.dy.clamp(-maxY, 0.0));
  }

  /// Keep the pointer on screen when zoomed in (touchpad mode).
  void _follow() {
    if (_zoom <= 1.01) return;
    final p = _toScreen(_cursor);
    const margin = 48.0;
    var dx = 0.0, dy = 0.0;
    if (p.dx < margin) dx = margin - p.dx;
    if (p.dx > _view.width - margin) dx = _view.width - margin - p.dx;
    if (p.dy < margin) dy = margin - p.dy;
    if (p.dy > _view.height - margin) dy = _view.height - margin - p.dy;
    _pan += Offset(dx, dy);
    _clampPan();
  }

  // ---- touch ----

  bool get _canInput => _hub.access?.input ?? false;

  void _moveTo(Offset remote) {
    _cursor = remote;
    _hub.session.pointer(x: remote.dx, y: remote.dy);
  }

  void _click(Button b) {
    _hub.session.button(button: b, down: true);
    _hub.session.button(button: b, down: false);
  }

  void _onDown(PointerDownEvent e) {
    _pointers[e.pointer] = e.localPosition;
    if (_pointers.length == 1) {
      _downPos = e.localPosition;
      _downAt = DateTime.now();
      _moved = false;
      _longPressed = false;
      _maxPointers = 1;
      _twoFinger = null;
      _longPress?.cancel();
      _longPress = Timer(const Duration(milliseconds: 450), _onLongPress);
    } else if (_pointers.length == 2) {
      _longPress?.cancel();
      _maxPointers = 2;
      if (_holding) {
        _hub.session.button(button: Button.left, down: false);
        _holding = false;
      }
      final pts = _pointers.values.toList();
      _startDist = math.max((pts[0] - pts[1]).distance, 1);
      _startCentroid = (pts[0] + pts[1]) / 2;
      _lastCentroid = _startCentroid;
      _startZoom = _zoom;
      _startPan = _pan;
      _twoFinger = null;
    } else {
      _maxPointers = math.max(_maxPointers, _pointers.length);
    }
  }

  void _onMove(PointerMoveEvent e) {
    if (!_pointers.containsKey(e.pointer)) return;
    _pointers[e.pointer] = e.localPosition;
    if (_pointers.length == 1 && _maxPointers == 1) {
      final total = e.localPosition - (_downPos ?? e.localPosition);
      if (!_moved && total.distance > 8) {
        _moved = true;
        _longPress?.cancel();
        if (_mode == TouchMode.direct && _canInput && !_holding) {
          _moveTo(_toRemote(_downPos!));
          _hub.session.button(button: Button.left, down: true);
          _holding = true;
        }
      }
      if (!_moved || !_canInput) return;
      if (_mode == TouchMode.touchpad) {
        final d = _dst;
        const speed = 1.6;
        final delta = Offset(e.delta.dx / (d.width * _zoom), e.delta.dy / (d.height * _zoom)) * speed;
        final next = _cursor + delta;
        _moveTo(Offset(next.dx.clamp(0.0, 1.0), next.dy.clamp(0.0, 1.0)));
        _follow();
      } else {
        _moveTo(_toRemote(e.localPosition));
      }
      setState(() {});
    } else if (_pointers.length == 2) {
      final pts = _pointers.values.toList();
      final dist = math.max((pts[0] - pts[1]).distance, 1.0);
      final centroid = (pts[0] + pts[1]) / 2;
      if (_twoFinger == null) {
        if ((dist / _startDist - 1).abs() > 0.08) {
          _twoFinger = 'zoom';
        } else if ((centroid - _startCentroid).distance > 12) {
          _twoFinger = 'scroll';
        }
      }
      if (_twoFinger == 'zoom') {
        final z = (_startZoom * dist / _startDist).clamp(1.0, 6.0);
        // Keep what was under the fingers under them.
        final content = (_startCentroid - _startPan) / _startZoom;
        _zoom = z;
        _pan = centroid - content * z;
        _clampPan();
        setState(() {});
      } else if (_twoFinger == 'scroll' && _canInput) {
        final d = centroid - _lastCentroid;
        const pxPerNotch = 36.0;
        _hub.session.scroll(dx: -d.dx / pxPerNotch, dy: d.dy / pxPerNotch);
      }
      _lastCentroid = centroid;
    }
  }

  void _onUp(PointerEvent e) {
    _pointers.remove(e.pointer);
    if (_pointers.isNotEmpty) return;
    _longPress?.cancel();
    final quick = DateTime.now().difference(_downAt) < const Duration(milliseconds: 350);
    if (!_canInput) {
      _holding = false;
      return;
    }
    if (_maxPointers == 1) {
      if (_holding) {
        _hub.session.button(button: Button.left, down: false);
        _holding = false;
      } else if (!_moved && !_longPressed && quick) {
        if (_mode == TouchMode.direct) _moveTo(_toRemote(_downPos!));
        _click(Button.left);
      }
    } else if (_maxPointers == 2 && _twoFinger == null && quick) {
      // Two-finger tap: right click.
      if (_mode == TouchMode.direct) _moveTo(_toRemote(_startCentroid));
      _click(Button.right);
    } else if (_maxPointers >= 3 && quick) {
      // Three-finger tap: the keyboard.
      _toggleKeyboard();
    }
    setState(() {});
  }

  void _onLongPress() {
    if (_pointers.length != 1 || _moved || !_canInput) return;
    _longPressed = true;
    HapticFeedback.mediumImpact();
    if (_mode == TouchMode.touchpad) {
      // Hold to drag: the button stays down until the finger lifts.
      _hub.session.button(button: Button.left, down: true);
      _holding = true;
    } else {
      _moveTo(_toRemote(_downPos!));
      _click(Button.right);
    }
  }

  void _onWheel(PointerSignalEvent e) {
    if (e is PointerScrollEvent && _canInput) {
      _hub.session.scroll(dx: -e.scrollDelta.dx / 40, dy: -e.scrollDelta.dy / 40);
    }
  }

  // ---- keyboard ----

  void _toggleKeyboard() {
    if (_kbFocus.hasFocus) {
      _kbFocus.unfocus();
      _hwFocus.requestFocus();
    } else {
      _kbFocus.requestFocus();
      SystemChannels.textInput.invokeMethod('TextInput.show');
    }
    setState(() {});
  }

  void _resetKb() {
    _kb.value = const TextEditingValue(text: _sentinel, selection: TextSelection.collapsed(offset: _sentinel.length));
  }

  void _onKeyboardText() {
    final v = _kb.value;
    if (v.composing.isValid && !v.composing.isCollapsed) return; // still composing a word
    final text = v.text;
    if (text == _sentinel) return;
    var common = 0;
    while (common < text.length && common < _sentinel.length && text[common] == _sentinel[common]) {
      common++;
    }
    final deleted = _sentinel.length - common;
    final typed = text.substring(common);
    _resetKb();
    if (!_canInput) return;
    for (var i = 0; i < deleted; i++) {
      _tapKey('Backspace');
    }
    if (typed.isNotEmpty) _type(typed);
  }

  void _type(String s) {
    if (_latched.isEmpty) {
      _hub.session.text(text: s);
      return;
    }
    // A shortcut: Ctrl/Alt/Win + the key that types this character.
    for (final ch in s.characters) {
      final code = codeForChar(ch);
      if (code != null) {
        _tapKey(code);
      } else {
        _hub.session.text(text: ch);
      }
    }
  }

  /// Press `code` with the latched modifiers, then let go of all.
  void _tapKey(String code) {
    final mods = _latched.toList();
    for (final m in mods) {
      _hub.session.key(code: m, down: true);
    }
    _hub.session.key(code: code, down: true);
    _hub.session.key(code: code, down: false);
    for (final m in mods.reversed) {
      _hub.session.key(code: m, down: false);
    }
    if (_latched.isNotEmpty) setState(() => _latched.clear());
  }

  void _combo(List<String> codes) {
    for (final c in codes) {
      _hub.session.key(code: c, down: true);
    }
    for (final c in codes.reversed) {
      _hub.session.key(code: c, down: false);
    }
  }

  KeyEventResult _onHardwareKey(FocusNode _, KeyEvent e) {
    // With the phone keyboard's field focused, typing arrives as text there instead.
    if (!_canInput || _kbFocus.hasFocus) return KeyEventResult.ignored;
    final code = physicalCodes[e.physicalKey];
    if (code == null) return KeyEventResult.ignored;
    if (e is KeyDownEvent || e is KeyRepeatEvent) {
      _hub.session.key(code: code, down: true);
    } else if (e is KeyUpEvent) {
      _hub.session.key(code: code, down: false);
    }
    return KeyEventResult.handled;
  }

  // ---- dialogs and panels ----

  Future<void> _askPassword(SessionEvent e) async {
    if (_passwordOpen) return;
    _passwordOpen = true;
    final field = TextEditingController();
    var remember = e.canRemember;
    final answer = await showDialog<(String, bool)?>(
      context: context,
      barrierDismissible: false,
      builder: (ctx) => StatefulBuilder(
        builder: (ctx, set) => AlertDialog(
          title: Text(t('Password for ${e.text}', 'Senha de ${e.text}')),
          content: Column(mainAxisSize: MainAxisSize.min, children: [
            if (e.detail.isNotEmpty)
              Padding(
                padding: const EdgeInsets.only(bottom: 8),
                child: Text(e.detail, style: TextStyle(color: Theme.of(ctx).colorScheme.error)),
              ),
            TextField(
              controller: field,
              obscureText: true,
              autofocus: true,
              autofillHints: const [AutofillHints.password],
              onSubmitted: (v) => Navigator.pop(ctx, (v, remember)),
            ),
            if (e.canRemember)
              CheckboxListTile(
                contentPadding: EdgeInsets.zero,
                value: remember,
                onChanged: (v) => set(() => remember = v ?? false),
                title: Text(t('Remember on this phone', 'Lembrar neste celular')),
              ),
          ]),
          actions: [
            TextButton(onPressed: () => Navigator.pop(ctx), child: Text(t('Cancel', 'Cancelar'))),
            FilledButton(onPressed: () => Navigator.pop(ctx, (field.text, remember)), child: Text(t('Sign in', 'Entrar'))),
          ],
        ),
      ),
    );
    _passwordOpen = false;
    _hub.session.answerPassword(password: answer?.$1, remember: answer?.$2 ?? false);
  }

  Future<void> _openPanel(Open what) async {
    _kbFocus.unfocus();
    final page = what == Open.files ? FilesPage(hub: _hub) : TerminalPage(hub: _hub);
    await Navigator.of(context).push(MaterialPageRoute(builder: (_) => page));
    // Back from a files- or terminal-only start: nothing more to do here.
    if (!_hub.video && mounted) Navigator.of(context).pop();
  }

  Future<void> _sendClipboard() async {
    final d = await Clipboard.getData(Clipboard.kTextPlain);
    final text = d?.text;
    if (text == null || text.isEmpty) {
      _snack(t('The phone\'s clipboard is empty.', 'A área de transferência do celular está vazia.'));
      return;
    }
    _hub.session.sendClipboard(text: text);
    _snack(t('Sent to the computer\'s clipboard.', 'Enviado para a área de transferência do computador.'));
  }

  // ---- build ----

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Focus(
      focusNode: _hwFocus,
      autofocus: true,
      onKeyEvent: _onHardwareKey,
      child: Scaffold(
        backgroundColor: Colors.black,
        resizeToAvoidBottomInset: true,
        body: SafeArea(
          child: Column(children: [
            if (_toolbar) _buildToolbar(theme),
            Expanded(
              child: Stack(children: [
                Positioned.fill(
                  child: LayoutBuilder(builder: (ctx, c) {
                    _view = Size(c.maxWidth, c.maxHeight);
                    return Listener(
                      behavior: HitTestBehavior.opaque,
                      onPointerDown: _onDown,
                      onPointerMove: _onMove,
                      onPointerUp: _onUp,
                      onPointerCancel: _onUp,
                      onPointerSignal: _onWheel,
                      child: CustomPaint(
                        painter: _ScreenPainter(
                          image: _image,
                          dst: _dst,
                          zoom: _zoom,
                          pan: _pan,
                          // Touchpad: the phone moves the pointer, so it always shows it. Direct
                          // touch: only where the computer says its pointer is.
                          cursor: _mode == TouchMode.touchpad || (_hub.cursorSupported && _cursorVisible) ? _cursor : null,
                        ),
                      ),
                    );
                  }),
                ),
                // The hidden field that brings up the phone keyboard.
                Positioned(
                  left: 0,
                  bottom: 0,
                  width: 1,
                  height: 1,
                  child: Opacity(
                    opacity: 0,
                    child: TextField(
                      controller: _kb,
                      focusNode: _kbFocus,
                      autocorrect: false,
                      enableSuggestions: false,
                      keyboardType: TextInputType.text,
                      textInputAction: TextInputAction.send,
                      onSubmitted: (_) {
                        if (_canInput) _tapKey('Enter');
                        _kbFocus.requestFocus();
                      },
                      onEditingComplete: () {},
                    ),
                  ),
                ),
                if (!_toolbar)
                  Positioned(
                    top: 0,
                    left: 0,
                    right: 0,
                    child: Center(
                      child: GestureDetector(
                        onTap: () => setState(() => _toolbar = true),
                        child: Container(
                          margin: const EdgeInsets.only(top: 2),
                          width: 64,
                          height: 18,
                          decoration: BoxDecoration(
                            color: Colors.white24,
                            borderRadius: BorderRadius.circular(9),
                          ),
                          child: const Icon(Icons.keyboard_arrow_down, size: 16, color: Colors.white70),
                        ),
                      ),
                    ),
                  ),
                if (!_hub.video) _overlay(theme, _pictureOff(theme)),
                if (_hub.video && (!_hub.connected || _image == null)) _overlay(theme, _statusCard(theme)),
              ]),
            ),
            if (_keysBar && _canInput) _buildKeysBar(theme),
          ]),
        ),
      ),
    );
  }

  Widget _overlay(ThemeData theme, Widget child) => Positioned.fill(
        child: Container(color: Colors.black54, alignment: Alignment.center, padding: const EdgeInsets.all(24), child: child),
      );

  Widget _pictureOff(ThemeData theme) => Card(
        child: Padding(
          padding: const EdgeInsets.all(20),
          child: Column(mainAxisSize: MainAxisSize.min, children: [
            Text(t('The screen is not shown in this session.', 'A tela não aparece nesta sessão.')),
            const SizedBox(height: 12),
            FilledButton(onPressed: _hub.showScreen, child: Text(t('Show the screen', 'Mostrar a tela'))),
          ]),
        ),
      );

  Widget _statusCard(ThemeData theme) {
    final fatal = _hub.fatal;
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(20),
        child: Column(mainAxisSize: MainAxisSize.min, children: [
          Text(widget.target.name, style: theme.textTheme.titleMedium),
          const SizedBox(height: 12),
          if (fatal == null) ...[
            const CircularProgressIndicator(),
            const SizedBox(height: 12),
            Text(_hub.status.isEmpty ? t('Connecting…', 'Conectando…') : _hub.status, textAlign: TextAlign.center),
          ] else
            Text(fatal, textAlign: TextAlign.center, style: TextStyle(color: theme.colorScheme.error)),
          const SizedBox(height: 16),
          Row(mainAxisSize: MainAxisSize.min, children: [
            TextButton(onPressed: () => Navigator.of(context).pop(), child: Text(t('Close', 'Fechar'))),
            const SizedBox(width: 8),
            FilledButton(onPressed: _hub.session.reconnect, child: Text(t('Try now', 'Tentar agora'))),
          ]),
        ]),
      ),
    );
  }

  Widget _buildToolbar(ThemeData theme) {
    final s = _hub.stats;
    final access = _hub.access;
    return Material(
      color: const Color(0xFF1D2128),
      child: SizedBox(
        height: 48,
        child: Row(children: [
          IconButton(
            tooltip: t('Disconnect', 'Desconectar'),
            icon: const Icon(Icons.close, color: Colors.white),
            onPressed: () => Navigator.of(context).pop(),
          ),
          Expanded(
            child: SingleChildScrollView(
              scrollDirection: Axis.horizontal,
              child: Row(children: [
                _tool(Icons.keyboard, t('Keyboard', 'Teclado'), _canInput ? _toggleKeyboard : null, on: _kbFocus.hasFocus),
                _tool(Icons.keyboard_command_key, t('Keys', 'Teclas'), _canInput ? () => setState(() => _keysBar = !_keysBar) : null,
                    on: _keysBar),
                _tool(
                  _mode == TouchMode.touchpad ? Icons.touch_app_outlined : Icons.pan_tool_alt_outlined,
                  _mode == TouchMode.touchpad ? t('Touchpad', 'Touchpad') : t('Direct touch', 'Toque direto'),
                  () => setState(() => _mode = _mode == TouchMode.touchpad ? TouchMode.direct : TouchMode.touchpad),
                ),
                if (_hub.monitors.length > 1)
                  PopupMenuButton<int>(
                    tooltip: t('Monitor', 'Tela'),
                    icon: const Icon(Icons.monitor, color: Colors.white),
                    onSelected: (id) => _hub.session.selectMonitor(id: id),
                    itemBuilder: (_) => [
                      for (final m in _hub.monitors)
                        CheckedPopupMenuItem(
                          value: m.id,
                          checked: m.id == _hub.activeMonitor,
                          child: Text('${m.name.isEmpty ? m.id : m.name} · ${m.width}×${m.height}'),
                        ),
                    ],
                  ),
                PopupMenuButton<String>(
                  tooltip: t('Quality', 'Qualidade'),
                  icon: const Icon(Icons.tune, color: Colors.white),
                  onSelected: _quality,
                  itemBuilder: (_) => [
                    PopupMenuItem(value: 'speed', child: Text(t('Speed (mobile data)', 'Velocidade (dados móveis)'))),
                    PopupMenuItem(value: 'auto', child: Text(t('Balanced', 'Equilibrada'))),
                    PopupMenuItem(value: 'quality', child: Text(t('Quality', 'Qualidade'))),
                    const PopupMenuDivider(),
                    PopupMenuItem(value: 'fit', child: Text(t('Resolution: fit the phone', 'Resolução: caber no celular'))),
                    PopupMenuItem(value: 'full', child: Text(t('Resolution: full', 'Resolução: completa'))),
                  ],
                ),
                if (_canInput) _tool(Icons.content_paste_go, t('Send the clipboard', 'Enviar a área de transferência'), _sendClipboard),
                if (access?.files ?? false) _tool(Icons.folder_outlined, t('Files', 'Arquivos'), () => _openPanel(Open.files)),
                if (access?.terminal ?? false) _tool(Icons.terminal, t('Terminal', 'Terminal'), () => _openPanel(Open.terminal)),
                if (_canInput)
                  PopupMenuButton<String>(
                    tooltip: t('Shortcuts', 'Atalhos'),
                    icon: const Icon(Icons.more_horiz, color: Colors.white),
                    onSelected: (v) => _combo(switch (v) {
                      'cad' => ['ControlLeft', 'AltLeft', 'Delete'],
                      'alttab' => ['AltLeft', 'Tab'],
                      'win' => ['MetaLeft'],
                      'esc' => ['Escape'],
                      'taskmgr' => ['ControlLeft', 'ShiftLeft', 'Escape'],
                      _ => <String>[],
                    }),
                    itemBuilder: (_) => const [
                      PopupMenuItem(value: 'cad', child: Text('Ctrl+Alt+Del')),
                      PopupMenuItem(value: 'alttab', child: Text('Alt+Tab')),
                      PopupMenuItem(value: 'win', child: Text('Win / ⌘')),
                      PopupMenuItem(value: 'taskmgr', child: Text('Ctrl+Shift+Esc')),
                      PopupMenuItem(value: 'esc', child: Text('Esc')),
                    ],
                  ),
              ]),
            ),
          ),
          if (s != null)
            Padding(
              padding: const EdgeInsets.symmetric(horizontal: 4),
              child: Text('${s.rttMs} ms\n${s.route}', textAlign: TextAlign.right, style: const TextStyle(color: Colors.white60, fontSize: 10)),
            ),
          IconButton(
            tooltip: t('Hide', 'Esconder'),
            icon: const Icon(Icons.keyboard_arrow_up, color: Colors.white),
            onPressed: () => setState(() => _toolbar = false),
          ),
        ]),
      ),
    );
  }

  void _quality(String v) {
    switch (v) {
      case 'speed':
        _hub.session.setQuality(maxFps: 15, jpegQuality: 45);
      case 'auto':
        _hub.session.setQuality(maxFps: 30, jpegQuality: 70);
      case 'quality':
        _hub.session.setQuality(maxFps: 30, jpegQuality: 90);
      case 'fit':
        final v = WidgetsBinding.instance.platformDispatcher.views.first.physicalSize;
        final long = math.max(v.width, v.height).round();
        _hub.session.setMaxSize(width: long, height: long);
      case 'full':
        _hub.session.setMaxSize(width: 0, height: 0);
    }
  }

  Widget _tool(IconData icon, String tip, VoidCallback? onTap, {bool on = false}) => IconButton(
        tooltip: tip,
        isSelected: on,
        icon: Icon(icon, color: onTap == null ? Colors.white24 : (on ? Colors.lightBlueAccent : Colors.white)),
        onPressed: onTap,
      );

  Widget _buildKeysBar(ThemeData theme) {
    Widget mod(String code, String label) {
      final on = _latched.contains(code);
      return Padding(
        padding: const EdgeInsets.symmetric(horizontal: 2),
        child: FilterChip(
          label: Text(label),
          selected: on,
          onSelected: (v) => setState(() => v ? _latched.add(code) : _latched.remove(code)),
        ),
      );
    }

    Widget key(String code, String label) => Padding(
          padding: const EdgeInsets.symmetric(horizontal: 2),
          child: ActionChip(label: Text(label), onPressed: () => _tapKey(code)),
        );

    return Material(
      color: const Color(0xFF1D2128),
      child: SizedBox(
        height: 52,
        child: ListView(scrollDirection: Axis.horizontal, padding: const EdgeInsets.symmetric(horizontal: 6, vertical: 6), children: [
          mod('ControlLeft', 'Ctrl'),
          mod('AltLeft', 'Alt'),
          mod('ShiftLeft', 'Shift'),
          mod('MetaLeft', 'Win/⌘'),
          key('Escape', 'Esc'),
          key('Tab', 'Tab'),
          key('ArrowLeft', '←'),
          key('ArrowUp', '↑'),
          key('ArrowDown', '↓'),
          key('ArrowRight', '→'),
          key('Home', 'Home'),
          key('End', 'End'),
          key('PageUp', 'PgUp'),
          key('PageDown', 'PgDn'),
          key('Delete', 'Del'),
          key('Insert', 'Ins'),
          for (var i = 1; i <= 12; i++) key('F$i', 'F$i'),
          key('PrintScreen', 'PrtSc'),
        ]),
      ),
    );
  }
}

class _ScreenPainter extends CustomPainter {
  _ScreenPainter({required this.image, required this.dst, required this.zoom, required this.pan, required this.cursor});

  final ui.Image? image;
  final Rect dst;
  final double zoom;
  final Offset pan;
  final Offset? cursor;

  @override
  void paint(Canvas canvas, Size size) {
    final img = image;
    if (img == null) return;
    canvas.save();
    canvas.translate(pan.dx, pan.dy);
    canvas.scale(zoom);
    final src = Rect.fromLTWH(0, 0, img.width.toDouble(), img.height.toDouble());
    canvas.drawImageRect(img, src, dst, Paint()..filterQuality = FilterQuality.medium);
    canvas.restore();
    final c = cursor;
    if (c != null) {
      final p = Offset(dst.left + c.dx * dst.width, dst.top + c.dy * dst.height) * zoom + pan;
      // An arrow pointer: white with a dark outline, so it shows on any background.
      final path = Path()
        ..moveTo(p.dx, p.dy)
        ..lineTo(p.dx, p.dy + 18)
        ..lineTo(p.dx + 4.5, p.dy + 13.5)
        ..lineTo(p.dx + 8, p.dy + 20.5)
        ..lineTo(p.dx + 10.5, p.dy + 19.5)
        ..lineTo(p.dx + 7, p.dy + 12.5)
        ..lineTo(p.dx + 13, p.dy + 12.5)
        ..close();
      canvas.drawPath(path, Paint()..color = Colors.white);
      canvas.drawPath(
        path,
        Paint()
          ..color = Colors.black
          ..style = PaintingStyle.stroke
          ..strokeWidth = 1.2,
      );
    }
  }

  @override
  bool shouldRepaint(_ScreenPainter old) =>
      old.image != image || old.dst != dst || old.zoom != zoom || old.pan != pan || old.cursor != cursor;
}
