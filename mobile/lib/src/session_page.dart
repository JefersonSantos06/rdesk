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
import 'help.dart';
import 'i18n.dart';
import 'keys.dart';
import 'prefs.dart';
import 'rust/api/session.dart';
import 'session_hub.dart';
import 'terminal_page.dart';
import 'touch.dart';

/// A remote session: the computer's screen, driven by touch ([TouchEngine]) or a mouse,
/// with the phone keyboard, a key bar, and the tools behind a floating button.
class SessionPage extends StatefulWidget {
  const SessionPage({super.key, required this.target, this.open = Open.screen});
  final Target target;
  final Open open;

  @override
  State<SessionPage> createState() => _SessionPageState();
}

/// A click shown where it happened.
class _Ripple {
  _Ripple(this.at, {required this.secondary});
  final Offset at;
  final bool secondary;
  final key = UniqueKey();
}

class _SessionPageState extends State<SessionPage> with WidgetsBindingObserver implements TouchActions {
  late final SessionHub _hub;
  StreamSubscription<SessionEvent>? _sub;

  // ---- picture ----
  ui.Image? _image;
  bool _decoding = false;
  bool _framePending = false;

  // ---- pointer ----
  late final TouchEngine _touch = TouchEngine(this)
    ..mode = Prefs.touchMode
    ..sensitivity = Prefs.sensitivity;
  final _clock = Stopwatch()..start();
  Timer? _touchTimer;
  bool _cursorVisible = true;
  final _ripples = <_Ripple>[];

  /// A mouse plugged into the phone: its buttons held now.
  int _mouseButtons = 0;

  // ---- keyboard ----
  static const _sentinel = '  ';
  final _kb = TextEditingController(text: _sentinel);
  final _kbFocus = FocusNode();
  final _hwFocus = FocusNode();
  bool _keysBar = false;
  final Set<String> _latched = {};

  // ---- chrome ----
  bool _passwordOpen = false;
  bool _helpShown = false;
  Turn _turn = Prefs.turn;
  bool _toolsLeft = Prefs.toolsLeft;
  double _toolsY = Prefs.toolsY;

  /// The tools button while it is being dragged.
  Offset? _toolsDrag;

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addObserver(this);
    final view = WidgetsBinding.instance.platformDispatcher.views.first;
    final long = (math.max(view.physicalSize.width, view.physicalSize.height)).round();
    _hub = SessionHub(widget.target, video: widget.open == Open.screen, maxWidth: long, maxHeight: long);
    _hub.addListener(_changed);
    _kb.addListener(_onKeyboardText);
    _kbFocus.addListener(_changed);
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
    _applyTurn();
    if (widget.open != Open.screen) {
      WidgetsBinding.instance.addPostFrameCallback((_) => _openPanel(widget.open));
    }
  }

  @override
  void dispose() {
    WidgetsBinding.instance.removeObserver(this);
    _touchTimer?.cancel();
    _sub?.cancel();
    _hub.removeListener(_changed);
    _hub.dispose();
    _image?.dispose();
    _kb.dispose();
    _kbFocus.dispose();
    _hwFocus.dispose();
    WakelockPlus.disable();
    SystemChrome.setEnabledSystemUIMode(SystemUiMode.edgeToEdge);
    SystemChrome.setPreferredOrientations([]);
    super.dispose();
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    if (state == AppLifecycleState.paused || state == AppLifecycleState.inactive) {
      // Never leave a key or button held on the computer.
      _hub.session.releaseAll();
      _touch.reset();
      _mouseButtons = 0;
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
        // An invisible pointer (another monitor, or unknown) keeps the phone's own idea.
        if (e.visible) _touch.hostCursor(Offset(e.x / 65535, e.y / 65535));
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
    ScaffoldMessenger.of(context)
      ..hideCurrentSnackBar()
      ..showSnackBar(SnackBar(content: Text(text), duration: const Duration(seconds: 3)));
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
      if (old == null) _firstPicture();
    }
    _decoding = false;
    if (_framePending) {
      _framePending = false;
      _decode();
    }
  }

  /// The first picture of the first session: show the gestures once.
  void _firstPicture() {
    if (_helpShown || Prefs.helpSeen) return;
    _helpShown = true;
    Prefs.helpSeen = true;
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted) showGestureHelp(context, _touch.mode);
    });
  }

  /// Where the picture sits in a view of `size` at zoom 1.
  Rect _fit(Size size) {
    final img = _image;
    if (img == null || size.isEmpty) return Offset.zero & size;
    final fit = applyBoxFit(BoxFit.contain, Size(img.width.toDouble(), img.height.toDouble()), size);
    return Alignment.center.inscribe(fit.destination, Offset.zero & size);
  }

  // ---- touch and mouse ----

  bool get _canInput => _hub.access?.input ?? false;

  Duration get _now => _clock.elapsed;

  @override
  void pointer(Offset remote) => _hub.session.pointer(x: remote.dx, y: remote.dy);

  @override
  void button(PointerButton b, bool down) => _hub.session.button(
        button: switch (b) {
          PointerButton.left => Button.left,
          PointerButton.right => Button.right,
          PointerButton.middle => Button.middle,
        },
        down: down,
      );

  @override
  void scroll(double dx, double dy) => _hub.session.scroll(dx: dx, dy: dy);

  @override
  void keyboard() => _toggleKeyboard();

  @override
  void feedback(TouchFeedback kind, Offset screen) {
    switch (kind) {
      case TouchFeedback.click || TouchFeedback.doubleClick:
        HapticFeedback.selectionClick();
      case TouchFeedback.rightClick || TouchFeedback.dragStart:
        HapticFeedback.mediumImpact();
      case TouchFeedback.aim:
        HapticFeedback.lightImpact();
        return;
    }
    final r = _Ripple(screen, secondary: kind == TouchFeedback.rightClick);
    _ripples.add(r);
    Timer(const Duration(milliseconds: 400), () {
      if (mounted) setState(() => _ripples.remove(r));
    });
  }

  /// After every touch: redraw, and come back when the engine has something to do later.
  void _afterTouch() {
    _touchTimer?.cancel();
    final d = _touch.deadline;
    if (d != null) {
      final wait = d - _now;
      _touchTimer = Timer(wait.isNegative ? Duration.zero : wait, () {
        _touch.tick(_now);
        _afterTouch();
      });
    }
    if (mounted) setState(() {});
  }

  /// Fingers go straight to a phone (the other side is a phone, and the person chose it):
  /// pinches and every other gesture happen there, as on that phone. Pointer → finger id.
  bool _fingersOn = true;
  final Map<int, (int, Offset)> _fingers = {};

  bool get _fingersMode => _fingersOn && _canInput && _hub.multitouchSupported;

  /// A finger touched, moved (`at`) or lifted (`at` null): tell the phone where they all are.
  void _finger(int pointer, Offset? at) {
    if (at == null) {
      _fingers.remove(pointer);
    } else {
      final id = _fingers[pointer]?.$1 ??
          List.generate(10, (i) => i).firstWhere((i) => !_fingers.values.any((f) => f.$1 == i), orElse: () => -1);
      if (id < 0) return;
      _fingers[pointer] = (id, _touch.toRemote(at));
    }
    _hub.session.touch(
      fingers: [for (final f in _fingers.values) Finger(id: f.$1, x: f.$2.dx, y: f.$2.dy)],
    );
  }

  void _onDown(PointerDownEvent e) {
    if (e.kind == PointerDeviceKind.mouse) return _mouse(e);
    if (_fingersMode) return _finger(e.pointer, e.localPosition);
    _touch.down(e.pointer, e.localPosition, _now);
    _afterTouch();
  }

  void _onMove(PointerMoveEvent e) {
    if (e.kind == PointerDeviceKind.mouse) return _mouse(e);
    if (_fingersMode) return _finger(e.pointer, e.localPosition);
    _touch.move(e.pointer, e.localPosition, _now, stamp: e.timeStamp);
    _afterTouch();
  }

  void _onUp(PointerUpEvent e) {
    if (e.kind == PointerDeviceKind.mouse) return _mouse(e);
    if (_fingersMode) return _finger(e.pointer, null);
    _touch.up(e.pointer, e.localPosition, _now);
    _afterTouch();
  }

  void _onCancel(PointerCancelEvent e) {
    if (e.kind == PointerDeviceKind.mouse) return _mouse(e);
    if (_fingersMode) return _finger(e.pointer, null);
    _touch.cancel(e.pointer, _now);
    _afterTouch();
  }

  void _onHover(PointerHoverEvent e) {
    if (e.kind == PointerDeviceKind.mouse) _mouse(e);
  }

  /// A mouse plugged into the phone points straight at the picture, as on a computer.
  void _mouse(PointerEvent e) {
    if (!_canInput) return;
    final remote = _touch.toRemote(e.localPosition);
    _touch.cursor = remote;
    pointer(remote);
    final changed = e.buttons ^ _mouseButtons;
    for (final (bit, b) in const [
      (kPrimaryMouseButton, PointerButton.left),
      (kSecondaryMouseButton, PointerButton.right),
      (kMiddleMouseButton, PointerButton.middle),
    ]) {
      if (changed & bit != 0) button(b, e.buttons & bit != 0);
    }
    _mouseButtons = e.buttons;
    setState(() {});
  }

  void _onWheel(PointerSignalEvent e) {
    if (e is PointerScrollEvent && _canInput) {
      _hub.session.scroll(dx: -e.scrollDelta.dx / 40, dy: -e.scrollDelta.dy / 40);
    }
  }

  // ---- keyboard ----

  bool get _keyboardOpen => MediaQuery.viewInsetsOf(context).bottom > 0;

  void _toggleKeyboard() {
    if (_kbFocus.hasFocus && _keyboardOpen) {
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
    HapticFeedback.selectionClick();
  }

  /// The key that goes with C, V, Z… for copy, paste, undo: ⌘ on a Mac, Ctrl elsewhere.
  String get _shortcutKey => _hub.hostOs == 'macos' ? 'MetaLeft' : 'ControlLeft';

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

  void _applyTurn() {
    SystemChrome.setPreferredOrientations(switch (_turn) {
      Turn.auto => [],
      Turn.landscape => [DeviceOrientation.landscapeLeft, DeviceOrientation.landscapeRight],
      Turn.portrait => [DeviceOrientation.portraitUp],
    });
  }

  String get _turnLabel => switch (_turn) {
        Turn.auto => t('Rotate: auto', 'Girar: automático'),
        Turn.landscape => t('Rotate: landscape', 'Girar: deitado'),
        Turn.portrait => t('Rotate: portrait', 'Girar: em pé'),
      };

  String get _modeLabel => _touch.mode == TouchMode.touchpad ? t('Touchpad', 'Touchpad') : t('Direct touch', 'Toque direto');

  /// The tools, in a sheet from the floating button.
  Future<void> _openTools() async {
    final access = _hub.access;
    final s = _hub.stats;
    final theme = Theme.of(context);
    final phone = _canInput && _hub.navSupported;
    final tiles = <(String, IconData, String, bool)>[
      if (phone) ('nav_back', Icons.arrow_back, t('Back', 'Voltar'), false),
      if (phone) ('nav_home', Icons.circle_outlined, t('Home', 'Início'), false),
      if (phone) ('nav_recents', Icons.crop_square, t('Recent apps', 'Recentes'), false),
      if (phone) ('nav_notifications', Icons.notifications_outlined, t('Notifications', 'Notificações'), false),
      if (_canInput && _hub.multitouchSupported)
        ('fingers', Icons.pan_tool_outlined, t('Touches go to the phone', 'Toques vão para o celular'), _fingersOn),
      if (_canInput) ('keyboard', Icons.keyboard, t('Keyboard', 'Teclado'), _kbFocus.hasFocus && _keyboardOpen),
      if (_canInput) ('keys', Icons.keyboard_command_key, t('Keys', 'Teclas'), _keysBar),
      (
        'mode',
        _touch.mode == TouchMode.touchpad ? Icons.mouse_outlined : Icons.touch_app_outlined,
        _modeLabel,
        false,
      ),
      ('help', Icons.help_outline, t('Gestures', 'Gestos'), false),
      if (_hub.monitors.length > 1) ('monitor', Icons.monitor, t('Screen', 'Tela'), false),
      ('quality', Icons.tune, t('Quality', 'Qualidade'), false),
      (
        'turn',
        switch (_turn) {
          Turn.auto => Icons.screen_rotation,
          Turn.landscape => Icons.stay_current_landscape,
          Turn.portrait => Icons.stay_current_portrait,
        },
        _turnLabel,
        false,
      ),
      if (_canInput) ('shortcuts', Icons.keyboard_alt_outlined, t('Shortcuts', 'Atalhos'), false),
      if (_canInput) ('clipboard', Icons.content_paste_go, t('Send clipboard', 'Enviar copiado'), false),
      if (access?.files ?? false) ('files', Icons.folder_outlined, t('Files', 'Arquivos'), false),
      if (access?.terminal ?? false) ('terminal', Icons.terminal, t('Terminal', 'Terminal'), false),
      ('close', Icons.close, t('Disconnect', 'Desconectar'), false),
    ];
    final action = await showModalBottomSheet<String>(
      context: context,
      showDragHandle: true,
      isScrollControlled: true,
      builder: (ctx) => SafeArea(
        child: SingleChildScrollView(
          padding: const EdgeInsets.fromLTRB(16, 0, 16, 16),
          child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
            Text(_hub.hostname.isEmpty ? widget.target.name : _hub.hostname, style: theme.textTheme.titleMedium),
            if (s != null)
              Text(
                [
                  '${s.rttMs} ms',
                  if (s.route.isNotEmpty) s.route,
                  '${s.fps.round()} ${t('fps', 'qps')}',
                ].join(' · '),
                style: theme.textTheme.bodySmall,
              ),
            const SizedBox(height: 12),
            Wrap(spacing: 8, runSpacing: 8, children: [
              for (final (key, icon, label, on) in tiles)
                _ToolTile(
                  icon: icon,
                  label: label,
                  on: on,
                  danger: key == 'close',
                  onTap: () => Navigator.pop(ctx, key),
                ),
            ]),
          ]),
        ),
      ),
    );
    if (!mounted || action == null) return;
    switch (action) {
      case 'fingers':
        setState(() {
          _fingersOn = !_fingersOn;
          _fingers.clear();
        });
        _snack(_fingersOn
            ? t('Your fingers touch the phone directly (pinch too)', 'Seus dedos tocam o celular direto (pinça também)')
            : t('Gestures as on a computer: pinch zooms here', 'Gestos como num computador: a pinça amplia aqui'));
      case 'nav_back':
        _hub.session.nav(button: NavButton.back);
      case 'nav_home':
        _hub.session.nav(button: NavButton.home);
      case 'nav_recents':
        _hub.session.nav(button: NavButton.recents);
      case 'nav_notifications':
        _hub.session.nav(button: NavButton.notifications);
      case 'keyboard':
        _toggleKeyboard();
      case 'keys':
        setState(() => _keysBar = !_keysBar);
      case 'mode':
        setState(() {
          _touch.mode = _touch.mode == TouchMode.touchpad ? TouchMode.direct : TouchMode.touchpad;
          Prefs.touchMode = _touch.mode;
        });
        _snack(_modeLabel);
      case 'help':
        showGestureHelp(context, _touch.mode);
      case 'monitor':
        _pickMonitor();
      case 'quality':
        _pickQuality();
      case 'turn':
        setState(() {
          _turn = Turn.values[(_turn.index + 1) % Turn.values.length];
          Prefs.turn = _turn;
        });
        _applyTurn();
        _snack(_turnLabel);
      case 'shortcuts':
        _pickShortcut();
      case 'clipboard':
        _sendClipboard();
      case 'files':
        _openPanel(Open.files);
      case 'terminal':
        _openPanel(Open.terminal);
      case 'close':
        Navigator.of(context).pop();
    }
  }

  Future<String?> _choose(String title, List<(String, String, bool)> options) => showDialog<String>(
        context: context,
        builder: (ctx) => SimpleDialog(
          title: Text(title),
          children: [
            for (final (value, label, on) in options)
              SimpleDialogOption(
                onPressed: () => Navigator.pop(ctx, value),
                child: Row(children: [
                  Expanded(child: Text(label)),
                  if (on) const Icon(Icons.check, size: 18),
                ]),
              ),
          ],
        ),
      );

  Future<void> _pickMonitor() async {
    final id = await _choose(t('Screen', 'Tela'), [
      for (final m in _hub.monitors)
        ('${m.id}', '${m.name.isEmpty ? m.id : m.name} · ${m.width}×${m.height}', m.id == _hub.activeMonitor),
    ]);
    if (id != null) _hub.session.selectMonitor(id: int.parse(id));
  }

  Future<void> _pickQuality() async {
    final v = await _choose(t('Quality', 'Qualidade'), [
      ('speed', t('Speed (mobile data)', 'Velocidade (dados móveis)'), false),
      ('auto', t('Balanced', 'Equilibrada'), false),
      ('quality', t('Quality', 'Qualidade'), false),
      ('fit', t('Resolution: fit the phone', 'Resolução: caber no celular'), false),
      ('full', t('Resolution: full', 'Resolução: completa'), false),
    ]);
    switch (v) {
      case 'speed':
        _hub.session.setQuality(maxFps: 15, jpegQuality: 45);
      case 'auto':
        _hub.session.setQuality(maxFps: 30, jpegQuality: 70);
      case 'quality':
        _hub.session.setQuality(maxFps: 30, jpegQuality: 90);
      case 'fit':
        final size = WidgetsBinding.instance.platformDispatcher.views.first.physicalSize;
        final long = math.max(size.width, size.height).round();
        _hub.session.setMaxSize(width: long, height: long);
      case 'full':
        _hub.session.setMaxSize(width: 0, height: 0);
    }
  }

  Future<void> _pickShortcut() async {
    final mac = _hub.hostOs == 'macos';
    final v = await _choose(t('Shortcuts', 'Atalhos'), [
      if (!mac) ('cad', 'Ctrl+Alt+Del', false),
      if (!mac) ('taskmgr', 'Ctrl+Shift+Esc', false),
      ('switch', mac ? '⌘+Tab' : 'Alt+Tab', false),
      if (mac) ('spotlight', t('⌘+Space (Spotlight)', '⌘+Espaço (Spotlight)'), false) else ('win', 'Win', false),
      ('esc', 'Esc', false),
    ]);
    if (v == null) return;
    _combo(switch (v) {
      'cad' => ['ControlLeft', 'AltLeft', 'Delete'],
      'taskmgr' => ['ControlLeft', 'ShiftLeft', 'Escape'],
      'switch' => [mac ? 'MetaLeft' : 'AltLeft', 'Tab'],
      'win' => ['MetaLeft'],
      'spotlight' => ['MetaLeft', 'Space'],
      'esc' => ['Escape'],
      _ => <String>[],
    });
  }

  // ---- build ----

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    _touch.input = _canInput;
    final keysBar = _canInput && (_keysBar || _keyboardOpen);
    return Focus(
      focusNode: _hwFocus,
      autofocus: true,
      onKeyEvent: _onHardwareKey,
      child: Scaffold(
        backgroundColor: Colors.black,
        resizeToAvoidBottomInset: true,
        body: SafeArea(
          child: Column(children: [
            Expanded(
              child: LayoutBuilder(builder: (ctx, c) {
                final size = Size(c.maxWidth, c.maxHeight);
                _touch.layout(size, _fit(size));
                // Touchpad: the phone moves the pointer, so it always shows it. Direct touch
                // (or a mouse): where the computer says its pointer is.
                final showCursor =
                    _image != null && (_touch.mode == TouchMode.touchpad || (_hub.cursorSupported && _cursorVisible));
                return Stack(children: [
                  Positioned.fill(
                    child: Listener(
                      behavior: HitTestBehavior.opaque,
                      onPointerDown: _onDown,
                      onPointerMove: _onMove,
                      onPointerUp: _onUp,
                      onPointerCancel: _onCancel,
                      onPointerHover: _onHover,
                      onPointerSignal: _onWheel,
                      child: CustomPaint(
                        painter: _ScreenPainter(
                          image: _image,
                          shown: _touch.shown,
                          cursor: showCursor ? _touch.toScreen(_touch.cursor) : null,
                          loupe: _image == null ? null : _touch.loupe,
                        ),
                      ),
                    ),
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
                  for (final r in _ripples)
                    Positioned(
                      key: r.key,
                      left: r.at.dx - 24,
                      top: r.at.dy - 24,
                      width: 48,
                      height: 48,
                      child: IgnorePointer(child: _RippleRing(secondary: r.secondary)),
                    ),
                  if (!_hub.video) _overlay(theme, _pictureOff(theme)),
                  if (_hub.video && (!_hub.connected || _image == null)) _overlay(theme, _statusCard(theme)),
                  _toolsButton(size),
                ]);
              }),
            ),
            if (keysBar) _buildKeysBar(theme),
          ]),
        ),
      ),
    );
  }

  /// The floating button for the tools: drag it anywhere, it rests against a side.
  Widget _toolsButton(Size area) {
    const d = 48.0, margin = 6.0;
    final room = math.max(area.height - d, 0.0);
    final at = _toolsDrag ?? Offset(_toolsLeft ? margin : area.width - d - margin, _toolsY * room);
    final rtt = _hub.stats?.rttMs;
    final dot = !_hub.connected
        ? Colors.redAccent
        : rtt == null
            ? Colors.white54
            : rtt < 80
                ? Colors.greenAccent
                : rtt < 200
                    ? Colors.amberAccent
                    : Colors.redAccent;
    return Positioned(
      left: at.dx,
      top: at.dy,
      child: GestureDetector(
        onTap: _openTools,
        onPanStart: (_) => setState(() => _toolsDrag = at),
        onPanUpdate: (u) => setState(() {
          final p = (_toolsDrag ?? at) + u.delta;
          _toolsDrag = Offset(p.dx.clamp(0.0, math.max(area.width - d, 0.0)), p.dy.clamp(0.0, room));
        }),
        onPanEnd: (_) {
          final p = _toolsDrag ?? at;
          setState(() {
            _toolsLeft = p.dx + d / 2 < area.width / 2;
            _toolsY = room > 0 ? p.dy / room : 0;
            _toolsDrag = null;
          });
          Prefs.setToolsAt(left: _toolsLeft, y: _toolsY);
        },
        child: Semantics(
          button: true,
          label: t('Tools', 'Ferramentas'),
          child: Container(
            width: d,
            height: d,
            decoration: BoxDecoration(
              color: Colors.black54,
              shape: BoxShape.circle,
              border: Border.all(color: Colors.white38),
            ),
            child: Stack(alignment: Alignment.center, children: [
              const Icon(Icons.apps, color: Colors.white),
              Positioned(
                right: 9,
                top: 9,
                child: Container(width: 8, height: 8, decoration: BoxDecoration(color: dot, shape: BoxShape.circle)),
              ),
            ]),
          ),
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

  /// Keys the phone keyboard lacks: shown above it whenever it is open, or on request.
  Widget _buildKeysBar(ThemeData theme) {
    final mac = _hub.hostOs == 'macos';
    Widget mod(String code, String label) {
      final on = _latched.contains(code);
      return Padding(
        padding: const EdgeInsets.symmetric(horizontal: 2),
        child: FilterChip(
          label: Text(label),
          selected: on,
          showCheckmark: false,
          onSelected: (v) => setState(() => v ? _latched.add(code) : _latched.remove(code)),
        ),
      );
    }

    Widget key(String code, String label) => Padding(
          padding: const EdgeInsets.symmetric(horizontal: 2),
          child: ActionChip(label: Text(label), onPressed: () => _tapKey(code)),
        );

    Widget shortcut(String code, IconData icon, String label) => Padding(
          padding: const EdgeInsets.symmetric(horizontal: 2),
          child: ActionChip(
            avatar: Icon(icon, size: 18),
            label: Text(label),
            onPressed: () => _combo([_shortcutKey, code]),
          ),
        );

    final gap = Container(width: 1, height: 24, margin: const EdgeInsets.symmetric(horizontal: 6, vertical: 8), color: Colors.white24);
    return Material(
      color: const Color(0xFF1D2128),
      child: SizedBox(
        height: 52,
        child: ListView(scrollDirection: Axis.horizontal, padding: const EdgeInsets.symmetric(horizontal: 6, vertical: 6), children: [
          if (_keyboardOpen)
            IconButton(
              tooltip: t('Hide the keyboard', 'Esconder o teclado'),
              icon: const Icon(Icons.keyboard_hide, color: Colors.white),
              onPressed: _toggleKeyboard,
            )
          else
            IconButton(
              tooltip: t('Show the keyboard', 'Mostrar o teclado'),
              icon: const Icon(Icons.keyboard, color: Colors.white),
              onPressed: _toggleKeyboard,
            ),
          mod('ControlLeft', 'Ctrl'),
          mod('AltLeft', mac ? '⌥' : 'Alt'),
          mod('ShiftLeft', 'Shift'),
          mod('MetaLeft', mac ? '⌘' : 'Win'),
          key('Escape', 'Esc'),
          key('Tab', 'Tab'),
          key('ArrowLeft', '←'),
          key('ArrowUp', '↑'),
          key('ArrowDown', '↓'),
          key('ArrowRight', '→'),
          gap,
          shortcut('KeyC', Icons.copy, t('Copy', 'Copiar')),
          shortcut('KeyV', Icons.paste, t('Paste', 'Colar')),
          shortcut('KeyX', Icons.content_cut, t('Cut', 'Recortar')),
          shortcut('KeyZ', Icons.undo, t('Undo', 'Desfazer')),
          shortcut('KeyA', Icons.select_all, t('All', 'Tudo')),
          gap,
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

/// One tool in the tools sheet.
class _ToolTile extends StatelessWidget {
  const _ToolTile({required this.icon, required this.label, required this.onTap, this.on = false, this.danger = false});
  final IconData icon;
  final String label;
  final VoidCallback onTap;
  final bool on;
  final bool danger;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final color = danger ? scheme.error : (on ? scheme.primary : scheme.onSurface);
    return SizedBox(
      width: 84,
      child: Material(
        color: on ? scheme.primaryContainer : scheme.surfaceContainerHighest,
        borderRadius: BorderRadius.circular(14),
        child: InkWell(
          borderRadius: BorderRadius.circular(14),
          onTap: onTap,
          child: Padding(
            padding: const EdgeInsets.symmetric(vertical: 12, horizontal: 4),
            child: Column(children: [
              Icon(icon, color: color),
              const SizedBox(height: 6),
              Text(label, textAlign: TextAlign.center, maxLines: 2, style: TextStyle(fontSize: 12, color: color)),
            ]),
          ),
        ),
      ),
    );
  }
}

/// A ring that grows and fades where a click happened (orange for the right button).
class _RippleRing extends StatelessWidget {
  const _RippleRing({required this.secondary});
  final bool secondary;

  @override
  Widget build(BuildContext context) => TweenAnimationBuilder<double>(
        tween: Tween(begin: 0, end: 1),
        duration: const Duration(milliseconds: 350),
        builder: (_, v, _) => Opacity(
          opacity: (1 - v) * 0.9,
          child: Transform.scale(
            scale: 0.35 + 0.65 * v,
            child: DecoratedBox(
              decoration: BoxDecoration(
                shape: BoxShape.circle,
                border: Border.all(color: secondary ? Colors.orangeAccent : Colors.white, width: 3),
              ),
            ),
          ),
        ),
      );
}

class _ScreenPainter extends CustomPainter {
  _ScreenPainter({required this.image, required this.shown, required this.cursor, required this.loupe});

  final ui.Image? image;

  /// Where the picture is drawn (zoom and pan applied).
  final Rect shown;

  /// The pointer, on screen.
  final Offset? cursor;

  /// The magnifier's target, on screen.
  final Offset? loupe;

  static const _loupeRadius = 60.0, _loupeZoom = 2.5, _loupeLift = 110.0;

  @override
  void paint(Canvas canvas, Size size) {
    final img = image;
    if (img == null) return;
    final src = Rect.fromLTWH(0, 0, img.width.toDouble(), img.height.toDouble());
    final paint = Paint()..filterQuality = FilterQuality.medium;
    canvas.drawImageRect(img, src, shown, paint);
    final c = cursor;
    if (c != null) _arrow(canvas, c);
    final target = loupe;
    if (target != null) _magnifier(canvas, size, img, src, paint, target);
  }

  /// An arrow pointer: white with a dark outline, so it shows on any background.
  void _arrow(Canvas canvas, Offset p) {
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

  /// A round magnifier above the finger (below it near the top), centered on what the
  /// finger points at, with a cross on the exact spot.
  void _magnifier(Canvas canvas, Size size, ui.Image img, Rect src, Paint paint, Offset target) {
    const r = _loupeRadius;
    var center = target - const Offset(0, _loupeLift);
    if (center.dy < r + 8) center = target + const Offset(0, _loupeLift);
    center = Offset(center.dx.clamp(r + 4, math.max(size.width - r - 4, r + 4)), center.dy);
    Rect around(Rect a) => Rect.fromLTRB(
          (a.left - target.dx) * _loupeZoom + center.dx,
          (a.top - target.dy) * _loupeZoom + center.dy,
          (a.right - target.dx) * _loupeZoom + center.dx,
          (a.bottom - target.dy) * _loupeZoom + center.dy,
        );
    final circle = Path()..addOval(Rect.fromCircle(center: center, radius: r));
    canvas.drawShadow(circle, Colors.black, 6, false);
    canvas.save();
    canvas.clipPath(circle);
    canvas.drawColor(Colors.black, BlendMode.src);
    canvas.drawImageRect(img, src, around(shown), paint);
    final cross = Paint()
      ..color = Colors.redAccent
      ..strokeWidth = 1.5;
    canvas.drawLine(center - const Offset(10, 0), center + const Offset(10, 0), cross);
    canvas.drawLine(center - const Offset(0, 10), center + const Offset(0, 10), cross);
    canvas.restore();
    canvas.drawCircle(
      center,
      r,
      Paint()
        ..style = PaintingStyle.stroke
        ..strokeWidth = 2
        ..color = Colors.white,
    );
  }

  @override
  bool shouldRepaint(_ScreenPainter old) =>
      old.image != image || old.shown != shown || old.cursor != cursor || old.loupe != loupe;
}
