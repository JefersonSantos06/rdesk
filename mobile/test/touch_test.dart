import 'dart:ui';

import 'package:flutter_test/flutter_test.dart';
import 'package:rdesk/src/touch.dart';

/// Records what the gestures sent, collapsing runs of pointer moves and scrolls.
class Rec implements TouchActions {
  final log = <String>[];
  Offset? pointerAt;
  double scrollX = 0, scrollY = 0;
  int keyboards = 0;

  void _add(String s) {
    if (log.isNotEmpty && log.last == s && (s == 'move' || s == 'scroll')) return;
    log.add(s);
  }

  @override
  void pointer(Offset remote) {
    pointerAt = remote;
    _add('move');
  }

  @override
  void button(PointerButton button, bool down) => _add('${button.name} ${down ? 'down' : 'up'}');

  @override
  void scroll(double dx, double dy) {
    scrollX += dx;
    scrollY += dy;
    _add('scroll');
  }

  @override
  void keyboard() {
    keyboards++;
    _add('keyboard');
  }

  @override
  void feedback(TouchFeedback kind, Offset screen) => _add('fb ${kind.name}');

  List<String> get buttons => log.where((s) => s.contains(' down') || s.contains(' up')).toList();
}

/// Fingers on a phone with a 400×800 view and a 16:9 picture across its width, and a clock
/// that calls the engine back when it asked to be (as the page's timer does).
class Phone {
  Phone({TouchMode mode = TouchMode.touchpad}) : rec = Rec() {
    e = TouchEngine(rec)..mode = mode;
    e.layout(const Size(400, 800), const Rect.fromLTWH(0, 287.5, 400, 225));
  }

  final Rec rec;
  late final TouchEngine e;
  Duration now = Duration.zero;

  void wait(int ms) {
    final end = now + Duration(milliseconds: ms);
    for (var i = 0; i < 100000; i++) {
      final d = e.deadline;
      if (d == null || d > end) break;
      if (d > now) now = d;
      e.tick(now);
    }
    now = end;
  }

  void down(int id, Offset p) => e.down(id, p, now);
  void up(int id, Offset p) => e.up(id, p, now);

  void moveTo(int id, Offset p, {int ms = 16}) {
    wait(ms);
    e.move(id, p, now);
  }

  /// Slide finger `id` from `from` to `to` in `steps` events `ms` apart.
  void slide(int id, Offset from, Offset to, {int steps = 10, int ms = 16}) {
    for (var i = 1; i <= steps; i++) {
      moveTo(id, Offset.lerp(from, to, i / steps)!, ms: ms);
    }
  }

  void tap(Offset p, {int ms = 60}) {
    down(1, p);
    wait(ms);
    up(1, p);
  }

  /// Fingers 1 and 2 down at `a` and `b`, moved by `by` (each), lifted.
  void two(Offset a, Offset b, {Offset by = Offset.zero, double spread = 0, int steps = 6, bool lift = true}) {
    down(1, a);
    down(2, b);
    final dir = (b - a) / (b - a).distance;
    for (var i = 1; i <= steps; i++) {
      final f = i / steps;
      wait(16);
      e.move(1, a + by * f - dir * spread * f / 2, now);
      e.move(2, b + by * f + dir * spread * f / 2, now);
    }
    if (lift) {
      up(1, a + by - dir * spread / 2);
      up(2, b + by + dir * spread / 2);
    }
  }
}

const center = Offset(200, 400);

void main() {
  group('touchpad', () {
    test('a tap clicks once no second tap follows', () {
      final p = Phone()..tap(center);
      expect(p.rec.buttons, isEmpty, reason: 'it waits for a possible second tap');
      p.wait(300);
      expect(p.rec.buttons, ['left down', 'left up']);
      expect(p.rec.log, contains('fb click'));
    });

    test('two taps are a double click, sent at once', () {
      final p = Phone()
        ..tap(center)
        ..wait(100)
        ..tap(center + const Offset(6, 4));
      expect(p.rec.buttons, ['left down', 'left up', 'left down', 'left up']);
      expect(p.rec.log, contains('fb doubleClick'));
      p.wait(500);
      expect(p.rec.buttons.length, 4);
    });

    test('tap, then touch and slide, drags without clicking first', () {
      final p = Phone()
        ..tap(center)
        ..wait(100)
        ..down(1, center)
        ..slide(1, center, center + const Offset(80, 0));
      expect(p.rec.buttons, ['left down']);
      p.up(1, center + const Offset(80, 0));
      p.wait(500);
      expect(p.rec.buttons, ['left down', 'left up']);
      expect(p.e.cursor.dx, greaterThan(0.5));
    });

    test('resting the finger before moving presses nothing', () {
      final p = Phone()
        ..down(1, center)
        ..wait(300)
        ..slide(1, center, center + const Offset(0, 60));
      p.up(1, center + const Offset(0, 60));
      p.wait(500);
      expect(p.rec.buttons, isEmpty);
      expect(p.e.cursor.dy, greaterThan(0.5));
    });

    test('touch and hold is the right button', () {
      final p = Phone()
        ..down(1, center)
        ..wait(600);
      expect(p.rec.buttons, ['right down', 'right up']);
      p.up(1, center);
      p.wait(500);
      expect(p.rec.buttons.length, 2);
    });

    test('a slow move is finer than the finger, a fast one goes further', () {
      double travel({required int steps}) {
        final p = Phone();
        p.e.cursor = const Offset(0.1, 0.5);
        p.down(1, center);
        p.slide(1, center, center + const Offset(100, 0), steps: steps);
        return p.e.cursor.dx - 0.1;
      }

      final slow = travel(steps: 50), fast = travel(steps: 5);
      // 100 of the picture's 400 pixels is a quarter of the screen.
      expect(slow, lessThan(0.25 * 0.7));
      expect(fast, greaterThan(0.25 * 1.5));
    });

    test('the zoomed view follows the pointer', () {
      final p = Phone();
      p.e.zoomAround(center, 3);
      final pan = p.e.pan;
      p.down(1, center);
      p.slide(1, center, center + const Offset(150, 0), steps: 5);
      expect(p.e.pan.dx, lessThan(pan.dx));
      expect(p.e.toScreen(p.e.cursor).dx, lessThanOrEqualTo(400 - 47));
    });

    test('two fingers scroll, and a flick keeps scrolling for a while', () {
      final p = Phone()..two(const Offset(180, 500), const Offset(220, 500), by: const Offset(0, -120));
      final during = p.rec.scrollY;
      expect(during, lessThan(-2), reason: 'fingers going up scroll down');
      expect(p.rec.buttons, isEmpty);
      p.wait(2000);
      expect(p.rec.scrollY, lessThan(during - 1));
      expect(p.e.deadline, isNull, reason: 'the fling stops');
    });

    test('two fingers reported one at a time still scroll (not a pinch)', () {
      final p = Phone()
        ..down(1, const Offset(180, 500))
        ..down(2, const Offset(220, 500));
      for (var i = 1; i <= 6; i++) {
        p.wait(16);
        final stamp = p.now;
        p.e.move(1, Offset(180, 500 - 20.0 * i), p.now, stamp: stamp);
        p.e.move(2, Offset(220, 500 - 20.0 * i), p.now + const Duration(milliseconds: 1), stamp: stamp);
      }
      expect(p.e.zoom, 1);
      expect(p.rec.scrollY, lessThan(-2));
    });

    test('a pinch with one finger still zooms', () {
      final p = Phone()
        ..down(1, const Offset(150, 400))
        ..down(2, const Offset(250, 400));
      for (var i = 1; i <= 8; i++) {
        p.wait(16);
        p.e.move(2, Offset(250 + 12.5 * i, 400), p.now, stamp: p.now);
      }
      p.up(2, const Offset(350, 400));
      expect(p.e.zoom, closeTo(2, 0.05));
      expect(p.rec.scrollY, 0);
    });

    test('a second finger ends a drag', () {
      final p = Phone()
        ..tap(center)
        ..wait(100)
        ..down(1, center)
        ..slide(1, center, center + const Offset(40, 0));
      p.down(2, center + const Offset(100, 0));
      expect(p.rec.buttons, ['left down', 'left up']);
    });
  });

  group('both modes', () {
    for (final mode in TouchMode.values) {
      test('${mode.name}: pinch zooms the view and sends nothing', () {
        final p = Phone(mode: mode)..two(const Offset(150, 400), const Offset(250, 400), spread: 100);
        expect(p.e.zoom, closeTo(2, 0.05));
        p.wait(500);
        expect(p.rec.log.where((s) => s != 'move'), isEmpty);
      });

      test('${mode.name}: two fingers tapped are the right button; twice, they zoom', () {
        final p = Phone(mode: mode)..two(const Offset(180, 400), const Offset(220, 400));
        expect(p.rec.buttons, isEmpty);
        p.wait(300);
        expect(p.rec.buttons, ['right down', 'right up']);

        p.two(const Offset(180, 400), const Offset(220, 400));
        p.wait(100);
        p.two(const Offset(180, 400), const Offset(220, 400));
        expect(p.e.zoom, 2.5);
        p.wait(500);
        expect(p.rec.buttons.length, 2, reason: 'no right click for the zoom');

        p.two(const Offset(180, 400), const Offset(220, 400));
        p.wait(100);
        p.two(const Offset(180, 400), const Offset(220, 400));
        expect(p.e.zoom, 1);
      });

      test('${mode.name}: three fingers tapped show the keyboard', () {
        final p = Phone(mode: mode)
          ..down(1, const Offset(150, 400))
          ..down(2, const Offset(200, 400))
          ..down(3, const Offset(250, 400))
          ..wait(80)
          ..up(1, const Offset(150, 400))
          ..up(2, const Offset(200, 400))
          ..up(3, const Offset(250, 400));
        expect(p.rec.keyboards, 1);
        p.wait(500);
        expect(p.rec.buttons, isEmpty);
      });

      test('${mode.name}: view only sends nothing but still zooms', () {
        final p = Phone(mode: mode);
        p.e.input = false;
        p
          ..tap(center)
          ..wait(600)
          ..down(1, center)
          ..wait(700)
          ..up(1, center)
          ..two(const Offset(150, 400), const Offset(250, 400), spread: 100);
        p.wait(500);
        expect(p.rec.log, isEmpty);
        expect(p.e.zoom, greaterThan(1.5));
      });
    }
  });

  group('direct', () {
    test('a tap clicks where the finger is, at once', () {
      final p = Phone(mode: TouchMode.direct);
      final target = p.e.toScreen(const Offset(0.25, 0.5));
      p.tap(target);
      expect(p.rec.buttons, ['left down', 'left up']);
      expect(p.rec.pointerAt!.dx, closeTo(0.25, 0.001));
    });

    test('a second tap nearby lands where the first did', () {
      final p = Phone(mode: TouchMode.direct)..tap(center);
      final first = p.rec.pointerAt;
      p
        ..wait(120)
        ..tap(center + const Offset(10, 6));
      expect(p.rec.pointerAt, first);
      expect(p.rec.buttons.length, 4);
    });

    test('one finger scrolls, pressing nothing', () {
      final p = Phone(mode: TouchMode.direct)
        ..down(1, center)
        ..slide(1, center, center + const Offset(0, 120), steps: 8);
      p.up(1, center + const Offset(0, 120));
      expect(p.rec.scrollY, greaterThan(2), reason: 'finger going down scrolls up');
      expect(p.rec.buttons, isEmpty);
    });

    test('touch and hold shows the magnifier; sliding then drags', () {
      final p = Phone(mode: TouchMode.direct)
        ..down(1, center)
        ..wait(600);
      expect(p.e.loupe, center);
      expect(p.rec.log, contains('fb aim'));
      expect(p.rec.buttons, isEmpty);
      p.slide(1, center, center + const Offset(60, 20));
      expect(p.rec.buttons, ['left down']);
      expect(p.e.loupe, center + const Offset(60, 20));
      p.up(1, center + const Offset(60, 20));
      expect(p.rec.buttons, ['left down', 'left up']);
      expect(p.e.loupe, isNull);
      expect(p.rec.pointerAt, p.e.toRemote(center + const Offset(60, 20)));
    });

    test('touch, hold and lift is the right button there', () {
      final p = Phone(mode: TouchMode.direct)
        ..down(1, const Offset(100, 350))
        ..wait(600)
        ..up(1, const Offset(100, 350));
      expect(p.rec.buttons, ['right down', 'right up']);
      expect(p.rec.pointerAt, p.e.toRemote(const Offset(100, 350)));
    });

    test('two fingers move a zoomed view', () {
      final p = Phone(mode: TouchMode.direct);
      p.e.zoomAround(center, 3);
      final pan = p.e.pan;
      p.two(const Offset(180, 400), const Offset(220, 400), by: const Offset(60, 0));
      expect(p.e.pan.dx, greaterThan(pan.dx + 30));
      expect(p.rec.scrollX, 0);
      expect(p.rec.buttons, isEmpty);
    });

    test('dragging at the edge of a zoomed view moves the view', () {
      final p = Phone(mode: TouchMode.direct);
      p.e.zoomAround(center, 3);
      final pan = p.e.pan;
      p
        ..down(1, center)
        ..wait(600)
        ..slide(1, center, const Offset(395, 400))
        ..wait(200);
      expect(p.e.pan.dx, lessThan(pan.dx - 50));
      expect(p.rec.buttons, ['left down']);
    });
  });

  test('the picture stays centered until it is bigger than the view', () {
    final p = Phone();
    p.e.zoomAround(center, 2);
    // 225 × 2 = 450 tall, in an 800 view: still centered vertically.
    expect(p.e.shown.center.dy, closeTo(400, 0.5));
    expect(p.e.shown.left, lessThanOrEqualTo(0));
    expect(p.e.shown.right, greaterThanOrEqualTo(400));
  });
}
