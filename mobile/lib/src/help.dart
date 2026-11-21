import 'package:material_ui/material_ui.dart';

import 'i18n.dart';
import 'touch.dart';

/// The gestures of each touch mode, in a sheet (shown once on the first session, and from
/// the tools and the settings).
Future<void> showGestureHelp(BuildContext context, TouchMode mode) => showModalBottomSheet<void>(
      context: context,
      isScrollControlled: true,
      showDragHandle: true,
      builder: (_) => _GestureHelp(mode: mode),
    );

class _GestureHelp extends StatefulWidget {
  const _GestureHelp({required this.mode});
  final TouchMode mode;

  @override
  State<_GestureHelp> createState() => _GestureHelpState();
}

class _GestureHelpState extends State<_GestureHelp> {
  late TouchMode _mode = widget.mode;

  List<(IconData, String, String)> get _rows => switch (_mode) {
        TouchMode.touchpad => [
            (Icons.swipe, t('Slide one finger', 'Deslize um dedo'), t('Move the pointer (slow is precise, fast goes far)', 'Move o ponteiro (devagar é preciso, rápido vai longe)')),
            (Icons.touch_app, t('Tap', 'Toque'), t('Click', 'Clique')),
            (Icons.ads_click, t('Tap twice', 'Dois toques'), t('Double click', 'Clique duplo')),
            (Icons.open_with, t('Tap, then touch again and slide', 'Toque, toque de novo e deslize'), t('Drag (select, move windows)', 'Arrastar (selecionar, mover janelas)')),
            (Icons.back_hand_outlined, t('Touch and hold', 'Toque e segure'), t('Right click', 'Botão direito')),
            (Icons.swap_vert, t('Slide two fingers', 'Deslize dois dedos'), t('Scroll', 'Rolar')),
            ..._common,
          ],
        TouchMode.direct => [
            (Icons.touch_app, t('Tap', 'Toque'), t('Click right there', 'Clique ali mesmo')),
            (Icons.swap_vert, t('Slide one finger', 'Deslize um dedo'), t('Scroll', 'Rolar')),
            (Icons.zoom_in, t('Touch and hold', 'Toque e segure'), t('A magnifier shows where you point', 'Uma lupa mostra onde você aponta')),
            (Icons.open_with, t('…then slide', '…e deslize'), t('Drag (select, move windows)', 'Arrastar (selecionar, mover janelas)')),
            (Icons.back_hand_outlined, t('…or lift', '…ou solte'), t('Right click', 'Botão direito')),
            (Icons.pan_tool_alt_outlined, t('Slide two fingers', 'Deslize dois dedos'), t('Move the zoomed view', 'Move a vista ampliada')),
            ..._common,
          ],
      };

  List<(IconData, String, String)> get _common => [
        (Icons.pinch, t('Pinch', 'Pinça'), t('Zoom in and out', 'Aumentar e diminuir')),
        (Icons.filter_2, t('Tap with two fingers', 'Toque com dois dedos'), t('Right click', 'Botão direito')),
        (Icons.zoom_out_map, t('Tap twice with two fingers', 'Dois toques com dois dedos'), t('Zoom in there, or back out', 'Amplia ali, ou volta')),
        (Icons.keyboard, t('Tap with three fingers', 'Toque com três dedos'), t('Show or hide the keyboard', 'Mostra ou esconde o teclado')),
      ];

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return SafeArea(
      child: SingleChildScrollView(
        padding: const EdgeInsets.fromLTRB(20, 0, 20, 16),
        child: Column(crossAxisAlignment: CrossAxisAlignment.stretch, children: [
          Text(t('Gestures', 'Gestos'), style: theme.textTheme.titleLarge),
          const SizedBox(height: 12),
          SegmentedButton<TouchMode>(
            segments: [
              ButtonSegment(value: TouchMode.touchpad, icon: const Icon(Icons.mouse_outlined), label: Text(t('Touchpad', 'Touchpad'))),
              ButtonSegment(value: TouchMode.direct, icon: const Icon(Icons.touch_app_outlined), label: Text(t('Direct touch', 'Toque direto'))),
            ],
            selected: {_mode},
            onSelectionChanged: (s) => setState(() => _mode = s.first),
          ),
          const SizedBox(height: 8),
          Text(
            _mode == TouchMode.touchpad
                ? t('The screen works like a laptop\'s touchpad: the pointer moves where you slide.',
                    'A tela funciona como o touchpad de um notebook: o ponteiro anda conforme você desliza.')
                : t('Touch the computer\'s screen where you want, as on a touch screen.',
                    'Toque na tela do computador onde quiser, como numa tela sensível ao toque.'),
            style: theme.textTheme.bodySmall,
          ),
          const SizedBox(height: 8),
          for (final (icon, gesture, action) in _rows)
            ListTile(
              contentPadding: EdgeInsets.zero,
              dense: true,
              leading: Icon(icon, color: theme.colorScheme.primary),
              title: Text(gesture, style: const TextStyle(fontWeight: FontWeight.w600)),
              subtitle: Text(action),
            ),
          const SizedBox(height: 8),
          FilledButton(onPressed: () => Navigator.pop(context), child: Text(t('Got it', 'Entendi'))),
        ]),
      ),
    );
  }
}
