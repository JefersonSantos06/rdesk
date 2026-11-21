/// Touch gestures → the remote pointer, buttons and wheel, and the local view (zoom, pan).
///
/// Pure logic: the session page feeds it the fingers and the time, calls [TouchEngine.tick]
/// at [TouchEngine.deadline], and does what [TouchActions] asks. The two modes follow what
/// phones and laptops already taught people:
///
/// * Touchpad (a laptop's touchpad): one finger moves the pointer, faster when the finger
///   is faster; tap clicks; tap twice clicks twice; tap then touch again and slide drags;
///   touch and hold is the right button; two fingers scroll.
/// * Direct (a touch screen): tap clicks where the finger is; one finger scrolls; touch and
///   hold shows a magnifier, and then sliding drags while lifting is the right button; two
///   fingers move the view.
///
/// In both: pinch zooms the view, two fingers tapped together are the right button, two
/// taps of two fingers zoom in or back out, and three fingers tapped show the keyboard.
library;

import 'dart:math' as math;
import 'dart:ui';

enum TouchMode { touchpad, direct }

enum PointerButton { left, right, middle }

/// What a gesture did, for the page to show (a ripple) and feel (a vibration).
enum TouchFeedback { click, doubleClick, rightClick, dragStart, aim }

/// What the gestures do on the remote computer and in the app.
abstract class TouchActions {
  /// Move the remote pointer to `remote` (0..1 across the monitor on each axis).
  void pointer(Offset remote);
  void button(PointerButton button, bool down);

  /// Wheel notches, positive = up / right (fractions are fine).
  void scroll(double dx, double dy);
  void keyboard();
  void feedback(TouchFeedback kind, Offset screen);
}

/// How far a finger may wander and still tap (logical pixels).
const touchSlop = 8.0;

/// Released sooner than this, a touch is a tap; held this long, a long press.
const longPress = Duration(milliseconds: 500);

/// A second tap within this is a double tap.
const doubleTapTime = Duration(milliseconds: 260);
const _doubleTapSlop = 40.0;

/// Two- and three-finger taps: all fingers lifted within this.
const _multiTap = Duration(milliseconds: 400);

/// Direct mode: a second tap this close lands where the first did (so the computer sees a
/// double click, not two clicks a few pixels apart).
const _snapTime = Duration(milliseconds: 400);
const _snapSlop = 24.0;

const _frame = Duration(milliseconds: 16);

/// Logical pixels of finger travel per wheel notch.
const _notch = 28.0;
const maxZoom = 6.0;
const _zoomStep = 2.5;

/// Touchpad: pointer speed against finger speed (logical pixels per millisecond). Slow
/// moves are precise (half of the finger's travel), fast ones cross the screen.
const _slowSpeed = 0.04, _fastSpeed = 1.2, _slowGain = 0.5, _fastGain = 2.8;

/// A fling below this speed (logical pixels per millisecond) stops; it slows down with
/// this time constant.
const _flingMin = 0.25, _flingStop = 0.03, _flingDecay = 325.0;

/// Direct mode, dragging at the edge of a zoomed view: the view follows.
const _edge = 32.0, _edgeStep = 8.0;

/// Keep the touchpad pointer this far inside a zoomed view.
const _followMargin = 48.0;

enum _One { none, touch, second, aim, moving, dragging, scrolling, panning, done }

enum _Multi { none, undecided, zoom, pan }

class _Finger {
  _Finger(this.start, this.at) : last = start;
  final Offset start;
  final Duration at;
  Offset last;
}

/// Finger velocity over the last ~100 ms.
class _Track {
  final _samples = <(Duration, Offset)>[];

  void add(Duration t, Offset p) {
    _samples.add((t, p));
    while (_samples.length > 2 && t - _samples.first.$1 > const Duration(milliseconds: 100)) {
      _samples.removeAt(0);
    }
  }

  void clear() => _samples.clear();

  /// Logical pixels per millisecond.
  Offset get velocity {
    if (_samples.length < 2) return Offset.zero;
    final (t0, p0) = _samples.first;
    final (t1, p1) = _samples.last;
    final ms = (t1 - t0).inMicroseconds / 1000;
    return ms <= 0 ? Offset.zero : (p1 - p0) / ms;
  }
}

double _ms(Duration d) => d.inMicroseconds / 1000;

class TouchEngine {
  TouchEngine(this.actions);

  final TouchActions actions;
  TouchMode mode = TouchMode.touchpad;

  /// Touchpad pointer speed, 0.5..2.
  double sensitivity = 1;

  /// The session may use the keyboard and mouse (else gestures only zoom and move the view).
  bool input = true;

  /// The view and where the picture sits in it at zoom 1.
  Size view = Size.zero;
  Rect picture = Rect.zero;
  double zoom = 1;
  Offset pan = Offset.zero;

  /// The remote pointer as the phone knows it (0..1).
  Offset cursor = const Offset(0.5, 0.5);

  /// Where the magnifier looks (direct mode, touch and hold), on screen.
  Offset? loupe;

  final _fingers = <int, _Finger>{};
  int _max = 0;
  Duration _gestureAt = Duration.zero;
  bool _far = false;
  _One _one = _One.none;
  _Multi _multi = _Multi.none;
  Duration? _longPressAt;
  Offset _aimFrom = Offset.zero;
  Duration _lastMoveAt = Duration.zero;
  double _speed = 0;
  final _track = _Track();

  /// A touchpad tap waiting to see whether a second one follows: (released at, where).
  (Duration, Offset)? _pendingTap;

  /// A two-finger tap waiting for a second one (released at, centroid).
  (Duration, Offset)? _pendingTwoTap;

  /// Direct mode's last tap: (when, where on screen, where on the computer).
  (Duration, Offset, Offset)? _lastTap;

  double _startDist = 1;
  Offset _startCentroid = Offset.zero;
  Offset _lastCentroid = Offset.zero;
  double _startZoom = 1;
  Offset _startPan = Offset.zero;

  Offset _flingV = Offset.zero;
  bool _flingPans = false;
  Duration _flingAt = Duration.zero;
  Duration? _edgeAt;

  /// When [tick] must be called next (null: nothing pending).
  Duration? get deadline {
    Duration? next;
    void take(Duration? t) {
      if (t != null && (next == null || t < next!)) next = t;
    }

    take(_longPressAt);
    if (_pendingTap case (final at, _)) take(at + doubleTapTime);
    if (_pendingTwoTap case (final at, _) when _fingers.isEmpty) take(at + doubleTapTime);
    if (_flingV != Offset.zero) take(_flingAt + _frame);
    take(_edgeAt);
    return next;
  }

  /// Something is held down on the computer.
  bool get dragging => _one == _One.dragging;

  // ---- geometry ----

  /// Where the picture is drawn on screen.
  Rect get shown => Rect.fromLTWH(
      picture.left * zoom + pan.dx, picture.top * zoom + pan.dy, picture.width * zoom, picture.height * zoom);

  Offset toRemote(Offset screen) {
    final r = shown;
    if (r.isEmpty) return cursor;
    return Offset(((screen.dx - r.left) / r.width).clamp(0.0, 1.0), ((screen.dy - r.top) / r.height).clamp(0.0, 1.0));
  }

  Offset toScreen(Offset remote) {
    final r = shown;
    return Offset(r.left + remote.dx * r.width, r.top + remote.dy * r.height);
  }

  /// The view or the picture changed size.
  void layout(Size view, Rect picture) {
    if (view == this.view && picture == this.picture) return;
    this.view = view;
    this.picture = picture;
    _clampPan();
  }

  /// The computer says where its pointer is: believed unless a finger is moving it.
  void hostCursor(Offset remote) {
    if (_fingers.isEmpty && _one != _One.dragging) cursor = remote;
  }

  /// Zoom to `z` keeping what is under `focus` under it.
  void zoomAround(Offset focus, double z) {
    final content = (focus - pan) / zoom;
    zoom = z.clamp(1.0, maxZoom);
    pan = focus - content * zoom;
    _clampPan();
  }

  /// The picture fills the view where it can and is centered where it cannot.
  void _clampPan() {
    double axis(double offset, double size, double viewSize, double start) {
      final s = size * zoom, o = start * zoom;
      if (s <= viewSize) return (viewSize - s) / 2 - o;
      return offset.clamp(viewSize - s - o, -o);
    }

    pan = Offset(axis(pan.dx, picture.width, view.width, picture.left), axis(pan.dy, picture.height, view.height, picture.top));
  }

  void _panBy(Offset d) {
    pan += d;
    _clampPan();
  }

  /// Keep the touchpad pointer inside a zoomed view.
  void _follow() {
    if (zoom <= 1.01) return;
    final p = toScreen(cursor);
    var d = Offset.zero;
    if (p.dx < _followMargin) d += Offset(_followMargin - p.dx, 0);
    if (p.dx > view.width - _followMargin) d += Offset(view.width - _followMargin - p.dx, 0);
    if (p.dy < _followMargin) d += Offset(0, _followMargin - p.dy);
    if (p.dy > view.height - _followMargin) d += Offset(0, view.height - _followMargin - p.dy);
    if (d != Offset.zero) _panBy(d);
  }

  // ---- actions ----

  void _click(PointerButton b) {
    actions.button(b, true);
    actions.button(b, false);
  }

  void _scrollBy(Offset d) {
    // Direct mode: the content follows the finger, so a notch is bigger when zoomed in.
    final notch = mode == TouchMode.direct ? _notch * zoom : _notch;
    actions.scroll(-d.dx / notch, d.dy / notch);
  }

  /// The touchpad pointer moves by the finger's travel, more when the finger is fast.
  void _moveCursor(Offset delta, Duration t) {
    final ms = math.max(_ms(t - _lastMoveAt), 1.0);
    _lastMoveAt = t;
    final v = delta.distance / ms;
    _speed = _speed == 0 ? v : _speed * 0.5 + v * 0.5;
    final x = ((_speed - _slowSpeed) / (_fastSpeed - _slowSpeed)).clamp(0.0, 1.0);
    final gain = (_slowGain + (_fastGain - _slowGain) * x * x * (3 - 2 * x)) * sensitivity;
    final r = shown;
    if (r.isEmpty) return;
    cursor = Offset(
      (cursor.dx + delta.dx * gain / r.width).clamp(0.0, 1.0),
      (cursor.dy + delta.dy * gain / r.height).clamp(0.0, 1.0),
    );
    actions.pointer(cursor);
    _follow();
  }

  /// A waiting touchpad tap is a click after all.
  void _flushTap() {
    if (_pendingTap == null) return;
    _pendingTap = null;
    _click(PointerButton.left);
    actions.feedback(TouchFeedback.click, toScreen(cursor));
  }

  /// A waiting two-finger tap is a right click after all.
  void _flushTwoTap() {
    final p = _pendingTwoTap;
    if (p == null) return;
    _pendingTwoTap = null;
    if (!input) return;
    if (mode == TouchMode.direct) {
      cursor = toRemote(p.$2);
      actions.pointer(cursor);
    }
    _click(PointerButton.right);
    actions.feedback(TouchFeedback.rightClick, mode == TouchMode.direct ? p.$2 : toScreen(cursor));
  }

  void _toggleZoom(Offset at) {
    if (zoom < 1.5) {
      zoomAround(at, _zoomStep);
    } else {
      zoom = 1;
      pan = Offset.zero;
      _clampPan();
    }
  }

  void _startFling(Offset v, {required bool pans, required Duration at}) {
    if (v.distance < _flingMin) return;
    _flingV = v;
    _flingPans = pans;
    _flingAt = at;
  }

  void _stopFling() => _flingV = Offset.zero;

  // ---- events ----

  void down(int id, Offset p, Duration t) {
    _stopFling();
    _batch.clear();
    if (_fingers.isEmpty) {
      _max = 0;
      _far = false;
      _gestureAt = t;
      _multi = _Multi.none;
      if (_pendingTwoTap case (final at, _) when t - at > doubleTapTime) _flushTwoTap();
    }
    _fingers[id] = _Finger(p, t);
    _max = math.max(_max, _fingers.length);
    if (_fingers.length == 1) {
      _oneDown(p, t);
      return;
    }
    // A second (or third) finger ends what one finger was doing.
    _endOne();
    _flushTap();
    if (_fingers.length == 2) {
      final pts = _fingers.values.map((f) => f.last).toList();
      _startDist = math.max((pts[0] - pts[1]).distance, 1);
      _startCentroid = _lastCentroid = (pts[0] + pts[1]) / 2;
      _startZoom = zoom;
      _startPan = pan;
      _multi = _Multi.undecided;
      _track
        ..clear()
        ..add(t, _startCentroid);
    } else {
      _flushTwoTap();
      _multi = _Multi.none;
    }
  }

  void _oneDown(Offset p, Duration t) {
    _one = _One.touch;
    _longPressAt = t + longPress;
    _speed = 0;
    _lastMoveAt = t;
    _track
      ..clear()
      ..add(t, p);
    if (mode == TouchMode.touchpad && input) {
      final pending = _pendingTap;
      if (pending != null && t - pending.$1 <= doubleTapTime && (p - pending.$2).distance <= _doubleTapSlop) {
        _pendingTap = null;
        _one = _One.second;
      } else {
        _flushTap();
      }
    }
  }

  /// One finger's gesture is cut short (another finger landed, or the touch was cancelled).
  void _endOne() {
    if (_one == _One.dragging) actions.button(PointerButton.left, false);
    _one = _One.none;
    _longPressAt = null;
    _edgeAt = null;
    loupe = null;
  }

  /// A finger moved. `stamp` is the event's own time stamp: the fingers of one touch-screen
  /// report share it, and two fingers are looked at together once both reported (one
  /// finger moved and the other not yet looks like a pinch).
  void move(int id, Offset p, Duration t, {Duration? stamp}) {
    final f = _fingers[id];
    if (f == null) return;
    if ((p - f.start).distance > _snapSlop) _far = true;
    if (_max == 1) {
      final prev = f.last;
      f.last = p;
      _oneMove(f, prev, p, t);
      return;
    }
    if (_max != 2 || _fingers.length != 2) {
      f.last = p;
      return;
    }
    final s = stamp ?? t;
    // The previous report had only one finger (the other did not move): use it as it is.
    if (_batch.isNotEmpty && s != _batchStamp) _twoMoveNow(t);
    _batchStamp = s;
    f.last = p;
    _batch.add(id);
    if (_batch.length == 2) _twoMoveNow(t);
  }

  Duration? _batchStamp;
  final _batch = <int>{};

  void _twoMoveNow(Duration t) {
    _batch.clear();
    _twoMove(t);
  }

  void _oneMove(_Finger f, Offset prev, Offset p, Duration t) {
    _track.add(t, p);
    final beyond = (p - f.start).distance > touchSlop;
    switch (_one) {
      case _One.touch:
        if (!beyond) return;
        _longPressAt = null;
        _flushTwoTap();
        if (!input) {
          _one = _One.panning;
        } else if (mode == TouchMode.touchpad) {
          _one = _One.moving;
          _lastMoveAt = t;
        } else {
          _one = _One.scrolling;
        }
      case _One.second:
        if (!beyond) return;
        _longPressAt = null;
        _one = _One.dragging;
        _lastMoveAt = t;
        actions.button(PointerButton.left, true);
        actions.feedback(TouchFeedback.dragStart, toScreen(cursor));
      case _One.aim:
        if ((p - _aimFrom).distance <= touchSlop) {
          loupe = p;
          cursor = toRemote(p);
          actions.pointer(cursor);
          return;
        }
        _one = _One.dragging;
        actions.button(PointerButton.left, true);
        actions.feedback(TouchFeedback.dragStart, p);
        _dragTo(p, t);
      case _One.moving:
        _moveCursor(p - prev, t);
      case _One.dragging:
        if (mode == TouchMode.touchpad) {
          _moveCursor(p - prev, t);
        } else {
          _dragTo(p, t);
        }
      case _One.scrolling:
        _scrollBy(p - prev);
      case _One.panning:
        _panBy(p - prev);
      case _One.none || _One.done:
        break;
    }
  }

  /// Direct mode, dragging: the pointer and the magnifier follow the finger, and a zoomed
  /// view follows a finger at its edge.
  void _dragTo(Offset p, Duration t) {
    loupe = p;
    cursor = toRemote(p);
    actions.pointer(cursor);
    _edgeAt = zoom > 1.01 && _edgeVector(p) != Offset.zero ? t + _frame : null;
  }

  Offset _edgeVector(Offset p) {
    var d = Offset.zero;
    if (p.dx < _edge) d += const Offset(_edgeStep, 0);
    if (p.dx > view.width - _edge) d += const Offset(-_edgeStep, 0);
    if (p.dy < _edge) d += const Offset(0, _edgeStep);
    if (p.dy > view.height - _edge) d += const Offset(0, -_edgeStep);
    return d;
  }

  void _twoMove(Duration t) {
    final pts = _fingers.values.map((f) => f.last).toList();
    final dist = math.max((pts[0] - pts[1]).distance, 1.0);
    final c = (pts[0] + pts[1]) / 2;
    _track.add(t, c);
    if (_multi == _Multi.undecided) {
      if ((dist / _startDist - 1).abs() > 0.08) {
        _multi = _Multi.zoom;
      } else if ((c - _startCentroid).distance > 10) {
        _multi = _Multi.pan;
      }
      if (_multi != _Multi.undecided) _flushTwoTap();
    }
    switch (_multi) {
      case _Multi.zoom:
        // Pinch: zoom, and move the view with the fingers (what was under them stays).
        final content = (_startCentroid - _startPan) / _startZoom;
        zoom = (_startZoom * dist / _startDist).clamp(1.0, maxZoom);
        pan = c - content * zoom;
        _clampPan();
      case _Multi.pan:
        // Two fingers: the wheel on a touchpad, the view on a touch screen.
        if (mode == TouchMode.touchpad && input) {
          _scrollBy(c - _lastCentroid);
        } else {
          _panBy(c - _lastCentroid);
        }
      case _Multi.none || _Multi.undecided:
        break;
    }
    _lastCentroid = c;
  }

  void up(int id, Offset p, Duration t) => _lift(id, p, t, cancelled: false);

  /// The system took the touch (a system gesture, a dialog): nothing is clicked.
  void cancel(int id, Duration t) => _lift(id, _fingers[id]?.last ?? Offset.zero, t, cancelled: true);

  void _lift(int id, Offset p, Duration t, {required bool cancelled}) {
    if (!_fingers.containsKey(id)) return;
    if (_batch.isNotEmpty) _twoMoveNow(t);
    final f = _fingers.remove(id)!;
    f.last = p;
    if (cancelled) {
      _endOne();
      _pendingTap = null;
      _pendingTwoTap = null;
      if (_fingers.isEmpty) _multi = _Multi.none;
      return;
    }
    if (_fingers.isNotEmpty) {
      // Lifting one of two fingers ends the pinch or the scroll; the other does nothing.
      if (_max == 2) _track.add(t, (_fingers.values.first.last + p) / 2);
      return;
    }
    if (_max == 1) {
      _track.add(t, p);
      _oneUp(f, t);
    } else if (_max == 2) {
      _twoUp(t);
    } else if (input && !_far && t - _gestureAt <= _multiTap) {
      actions.keyboard();
    }
    _one = _One.none;
    _multi = _Multi.none;
    _longPressAt = null;
    _edgeAt = null;
    loupe = null;
  }

  void _oneUp(_Finger f, Duration t) {
    // A two-finger tap before it was not the first of two: its right click goes first.
    _flushTwoTap();
    switch (_one) {
      case _One.touch:
        if (!input) return;
        if (mode == TouchMode.touchpad) {
          _pendingTap = (t, f.start);
          return;
        }
        // Direct: click where the finger was (where the last tap was, if this is its double).
        var remote = toRemote(f.start);
        final last = _lastTap;
        if (last != null && t - last.$1 <= _snapTime && (f.start - last.$2).distance <= _snapSlop) remote = last.$3;
        _lastTap = (t, f.start, remote);
        cursor = remote;
        actions.pointer(remote);
        _click(PointerButton.left);
        actions.feedback(TouchFeedback.click, f.start);
      case _One.second:
        _click(PointerButton.left);
        if (t - f.at < longPress) {
          _click(PointerButton.left);
          actions.feedback(TouchFeedback.doubleClick, toScreen(cursor));
        } else {
          actions.feedback(TouchFeedback.click, toScreen(cursor));
        }
      case _One.dragging:
        actions.button(PointerButton.left, false);
      case _One.aim:
        _click(PointerButton.right);
        actions.feedback(TouchFeedback.rightClick, f.last);
      case _One.scrolling:
        _startFling(_track.velocity, pans: false, at: t);
      case _One.panning:
        _startFling(_track.velocity, pans: true, at: t);
      case _One.none || _One.moving || _One.done:
        break;
    }
  }

  void _twoUp(Duration t) {
    if (_multi == _Multi.undecided && !_far && t - _gestureAt <= _multiTap) {
      final pending = _pendingTwoTap;
      if (pending != null) {
        _pendingTwoTap = null;
        _toggleZoom(_lastCentroid);
      } else {
        _pendingTwoTap = (t, _lastCentroid);
      }
    } else if (_multi == _Multi.pan) {
      _startFling(_track.velocity, pans: !(mode == TouchMode.touchpad && input), at: t);
    }
  }

  /// Time passed: long presses, taps that turned out single, flings, dragging at an edge.
  void tick(Duration t) {
    final lp = _longPressAt;
    if (lp != null && t >= lp) {
      _longPressAt = null;
      _onLongPress();
    }
    if (_pendingTwoTap case (final at, _) when _fingers.isEmpty && t - at >= doubleTapTime) _flushTwoTap();
    if (_pendingTap case (final at, _) when t - at >= doubleTapTime) _flushTap();
    if (_flingV != Offset.zero && t > _flingAt) {
      final ms = _ms(t - _flingAt);
      final d = _flingV * ms;
      if (_flingPans) {
        _panBy(d);
      } else {
        _scrollBy(d);
      }
      _flingV *= math.exp(-ms / _flingDecay);
      if (_flingV.distance < _flingStop) _flingV = Offset.zero;
      _flingAt = t;
    }
    final edgeAt = _edgeAt;
    if (edgeAt != null && t >= edgeAt && _one == _One.dragging && _fingers.length == 1) {
      final p = _fingers.values.first.last;
      _panBy(_edgeVector(p));
      _dragTo(p, t);
    }
  }

  void _onLongPress() {
    if (_fingers.length != 1 || _max != 1 || !input) return;
    final f = _fingers.values.first;
    switch (_one) {
      case _One.touch:
        _flushTwoTap();
        if (mode == TouchMode.touchpad) {
          _one = _One.done;
          _click(PointerButton.right);
          actions.feedback(TouchFeedback.rightClick, toScreen(cursor));
        } else {
          _one = _One.aim;
          _aimFrom = f.last;
          loupe = f.last;
          cursor = toRemote(f.last);
          actions.pointer(cursor);
          actions.feedback(TouchFeedback.aim, f.last);
        }
      case _One.second:
        _one = _One.dragging;
        _lastMoveAt = f.at + longPress;
        actions.button(PointerButton.left, true);
        actions.feedback(TouchFeedback.dragStart, toScreen(cursor));
      default:
        break;
    }
  }

  /// The app went to the background: forget the gesture (the page releases what is held).
  void reset() {
    _fingers.clear();
    _one = _One.none;
    _multi = _Multi.none;
    _longPressAt = null;
    _pendingTap = null;
    _pendingTwoTap = null;
    _edgeAt = null;
    _flingV = Offset.zero;
    loupe = null;
  }
}
