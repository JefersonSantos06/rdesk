import 'dart:async';

import 'package:flutter/foundation.dart';

import 'rust/api/session.dart';

/// One session shared by the picture, files and terminal pages: the Rust session, its
/// events as a broadcast stream, and what is known so far.
class SessionHub extends ChangeNotifier {
  SessionHub(this.target, {required this.video, required int maxWidth, required int maxHeight})
      : session = RdSession(
          target: target,
          options: SessionOptions(maxWidth: maxWidth, maxHeight: maxHeight, video: video),
        );

  final Target target;
  final RdSession session;
  bool video;

  final _events = StreamController<SessionEvent>.broadcast();
  StreamSubscription<SessionEvent>? _sub;

  Stream<SessionEvent> get events => _events.stream;

  String status = '';
  bool connected = false;
  String? fatal;
  String hostname = '';
  Access? access;
  List<Monitor> monitors = [];
  int activeMonitor = 0;
  Stats? stats;
  bool cursorSupported = false;
  bool scalingSupported = false;
  bool closed = false;

  /// Connect (call once).
  void start() {
    _sub = session.start().listen((e) {
      _apply(e);
      if (!_events.isClosed) _events.add(e);
    }, onDone: () {
      closed = true;
      notifyListeners();
    });
  }

  void _apply(SessionEvent e) {
    switch (e.kind) {
      case EventKind.status:
        status = e.text;
        connected = e.ok;
        if (e.ok) fatal = null;
      case EventKind.connected:
        hostname = e.text;
        if (e.monitors.isNotEmpty) {
          monitors = e.monitors;
          activeMonitor = e.activeMonitor;
        }
      case EventKind.access:
        access = e.access;
      case EventKind.monitors:
        monitors = e.monitors;
        activeMonitor = e.activeMonitor;
      case EventKind.stats:
        stats = e.stats;
      case EventKind.fatal:
        fatal = e.text;
        connected = false;
      case EventKind.reconnecting:
        connected = false;
        status = e.text;
      case EventKind.extensions:
        cursorSupported = e.cursorSupported;
        scalingSupported = e.scalingSupported;
      case EventKind.closed:
        closed = true;
      default:
        return;
    }
    notifyListeners();
  }

  void showScreen() {
    if (!video) {
      video = true;
      session.showScreen();
      notifyListeners();
    }
  }

  @override
  void dispose() {
    session.close();
    _sub?.cancel();
    _events.close();
    super.dispose();
  }
}
