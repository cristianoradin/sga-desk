// ConectDesk widget canto desktop. Sempre on-top, frameless, 320x140 no canto inferior direito.
// Estados:
//   - idle:       só logo + brand_name
//   - em sessão:  foto técnico + nome + "Em atendimento" + barra verde animada
//   - pendente:   ícone escudo + "Aguardando aprovação"
// Lê options atualizados pelo agent (cd_active_session_*, cd_brand_*) via mainGetOptionSync,
// repolling a cada 1.5s pra capturar mudanças (não há sinal push pra sub-window).
import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:ui' show FontFeature;

import 'package:desktop_multi_window/desktop_multi_window.dart';
import 'package:flutter/material.dart';
import 'package:flutter_hbb/models/platform_model.dart';
import 'package:window_manager/window_manager.dart';
import 'package:window_size/window_size.dart' as window_size;

class CdWidgetPage extends StatefulWidget {
  final int windowId;
  const CdWidgetPage({Key? key, required this.windowId}) : super(key: key);
  @override
  State<CdWidgetPage> createState() => _CdWidgetPageState();
}

class _CdWidgetPageState extends State<CdWidgetPage> with SingleTickerProviderStateMixin {
  Timer? _poll;
  Timer? _tick;
  String _techName = '';
  String _techPhotoPath = '';
  String _brandName = '';
  String _brandLogoPath = '';
  String _sessionId = '';
  DateTime? _sessionStart; // marcado quando uma sessão nova aparece (≈ início real)
  Duration _elapsed = Duration.zero;
  bool _collapsed = false;
  int _photoTag = 0; // mtime^size do session_tech.png — força re-load do Image quando a foto chega
  List<Map<String, String>> _techs = []; // multi-técnico: [{name, photoPath}]
  late final AnimationController _fade;
  late final Animation<double> _fadeAnim;

  @override
  void initState() {
    super.initState();
    _fade = AnimationController(vsync: this, duration: const Duration(milliseconds: 320));
    _fadeAnim = CurvedAnimation(parent: _fade, curve: Curves.easeOutCubic);
    _refresh();
    _poll = Timer.periodic(const Duration(milliseconds: 1500), (_) => _refresh());
    _tick = Timer.periodic(const Duration(seconds: 1), (_) {
      if (_sessionStart != null && mounted) {
        setState(() => _elapsed = DateTime.now().difference(_sessionStart!));
      }
    });
    Future.delayed(const Duration(milliseconds: 60), () { if (mounted) _fade.forward(); });
  }

  @override
  void dispose() {
    _poll?.cancel();
    _tick?.cancel();
    _fade.dispose();
    super.dispose();
  }

  String _fmtElapsed() {
    final h = _elapsed.inHours;
    final m = _elapsed.inMinutes.remainder(60).toString().padLeft(2, '0');
    final s = _elapsed.inSeconds.remainder(60).toString().padLeft(2, '0');
    return h > 0 ? '${h}h ${m}m ${s}s' : '$m:$s';
  }

  void _refresh() {
    final n = bind.mainGetOptionSync(key: 'cd_active_session_tech_name');
    final p = bind.mainGetOptionSync(key: 'cd_active_session_tech_photo_path');
    final s = bind.mainGetOptionSync(key: 'cd_active_session_id');
    final bn = bind.mainGetOptionSync(key: 'cd_brand_name');
    final bp = bind.mainGetOptionSync(key: 'cd_brand_logo_path');
    // Multi-técnico: lista completa de técnicos conectados.
    final raw = bind.mainGetOptionSync(key: 'cd_active_sessions');
    List<Map<String, String>> techs = [];
    if (raw.isNotEmpty) {
      try {
        final parsed = jsonDecode(raw);
        if (parsed is List) {
          techs = parsed.map<Map<String, String>>((e) => {
            'name': (e['name'] ?? '').toString(),
            'photoPath': (e['photoPath'] ?? '').toString(),
          }).toList();
        }
      } catch (_) {}
    }
    if (techs.length != _techs.length ||
        techs.asMap().entries.any((e) => _techs.length <= e.key || _techs[e.key]['name'] != e.value['name'] || _techs[e.key]['photoPath'] != e.value['photoPath'])) {
      _techs = techs;
      if (mounted) setState(() {});
    }
    // O path da foto é sempre o mesmo arquivo (session_tech.png) — o agent o cria DEPOIS que o
    // widget abriu, então o option não muda. Checamos mtime+size do arquivo pra re-renderizar a
    // imagem quando ela finalmente chega (cache-bust via _photoTag).
    int photoStamp = 0;
    if (p.isNotEmpty) {
      try { final st = File(p).statSync(); photoStamp = st.modified.millisecondsSinceEpoch ^ st.size; } catch (_) {}
    }
    if (n != _techName || p != _techPhotoPath || s != _sessionId ||
        bn != _brandName || bp != _brandLogoPath || photoStamp != _photoTag) {
      // Foto mudou no disco → tira do cache do Flutter pra Image.file re-ler (senão serve a velha).
      if (photoStamp != _photoTag && p.isNotEmpty) {
        try { FileImage(File(p)).evict(); } catch (_) {}
      }
      if (s.isNotEmpty && s != _sessionId) {
        _sessionStart = DateTime.now();
        _elapsed = Duration.zero;
      } else if (s.isEmpty) {
        _sessionStart = null;
        _elapsed = Duration.zero;
      }
      setState(() {
        _techName = n; _techPhotoPath = p; _sessionId = s;
        _brandName = bn; _brandLogoPath = bp; _photoTag = photoStamp;
      });
    }
  }

  @override
  Widget build(BuildContext context) {
    final hasSession = _sessionId.isNotEmpty;
    final brand = _brandName.isNotEmpty ? _brandName : 'SGA Petro';
    // Sub-window do Windows NÃO é transparente (sem WS_EX_LAYERED no runner), então usamos
    // o próprio verde como fundo da janela — os cantos arredondados somem no verde em vez de
    // mostrar preto. Recolhido = pílula verde horizontal (não bolinha branca, que exigiria
    // transparência real).
    if (_collapsed) {
      return Scaffold(
        backgroundColor: const Color(0xff01A862),
        body: _collapsedBody(brand),
      );
    }
    return Scaffold(
        backgroundColor: const Color(0xff0A6A3A),
        body: GestureDetector(
          onPanStart: (_) async { try { await windowManager.startDragging(); } catch (_) {} },
          child: FadeTransition(
            opacity: _fadeAnim,
            child: ScaleTransition(
              scale: Tween<double>(begin: 0.92, end: 1.0).animate(_fadeAnim),
              child: Container(
            decoration: const BoxDecoration(
              gradient: LinearGradient(
                begin: Alignment.topLeft, end: Alignment.bottomRight,
                colors: [Color(0xff0A6A3A), Color(0xff01A862)],
              ),
            ),
            child: Padding(
              padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 12),
              child: hasSession ? _sessionBody(brand) : _idleBody(brand),
            ),
          ),
        ),
        ),
      ),
    );
  }

  Widget _idleBody(String brand) {
    return Row(
      children: [
        _brandLogo(56),
        const SizedBox(width: 12),
        Expanded(
          child: Column(
            mainAxisAlignment: MainAxisAlignment.center,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text(brand, style: const TextStyle(color: Colors.white, fontSize: 16, fontWeight: FontWeight.w700)),
              const SizedBox(height: 4),
              const Text('ConectDesk', style: TextStyle(color: Color(0xCCFFFFFF), fontSize: 12)),
              const SizedBox(height: 6),
              Row(children: [
                Container(width: 8, height: 8, decoration: const BoxDecoration(color: Color(0xff7CFF9C), shape: BoxShape.circle)),
                const SizedBox(width: 6),
                const Text('Pronto para conexão', style: TextStyle(color: Colors.white, fontSize: 11)),
              ]),
            ],
          ),
        ),
        _closeBtn(),
      ],
    );
  }

  Widget _sessionBody(String brand) {
    // Dedup por nome — várias conexões do MESMO técnico não devem aparecer repetidas
    // ("cristiano, cristiano, cristiano"). Conta técnicos DISTINTOS.
    final seen = <String>{};
    final uniqueTechs = <Map<String, String>>[];
    for (final t in _techs) {
      final n = (t['name'] ?? '').trim();
      if (n.isEmpty) continue;
      if (seen.add(n.toLowerCase())) uniqueTechs.add(t);
    }
    // 2+ técnicos DISTINTOS: layout multi. 1 ou 0: layout single.
    final multi = uniqueTechs.length > 1;
    final Widget left;
    final Widget info;
    if (multi) {
      // Avatares sobrepostos (até 3 visíveis).
      final shown = uniqueTechs.take(3).toList();
      left = SizedBox(
        width: 56, height: 56,
        child: Stack(
          children: [
            for (int i = 0; i < shown.length; i++)
              Positioned(
                left: i * 16.0,
                top: i * 6.0,
                child: _avatarFor(shown[i]['name'] ?? '', shown[i]['photoPath'] ?? '', 36),
              ),
          ],
        ),
      );
      final names = uniqueTechs.map((t) => t['name'] ?? '').where((s) => s.isNotEmpty).join(', ');
      info = Column(
        mainAxisAlignment: MainAxisAlignment.center,
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text('${uniqueTechs.length} técnicos', style: const TextStyle(color: Colors.white, fontSize: 15, fontWeight: FontWeight.w700)),
          const SizedBox(height: 2),
          Text(names, style: const TextStyle(color: Color(0xCCFFFFFF), fontSize: 11), maxLines: 2, overflow: TextOverflow.ellipsis),
          const SizedBox(height: 4),
          Text('Em atendimento · ${_fmtElapsed()}', style: const TextStyle(color: Color(0xEEFFFFFF), fontSize: 11, fontWeight: FontWeight.w700, fontFeatures: [FontFeature.tabularFigures()])),
        ],
      );
    } else {
      final techDisplay = _techName.isNotEmpty ? _techName : 'Técnico';
      left = _techAvatar(56);
      info = Column(
        mainAxisAlignment: MainAxisAlignment.center,
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(techDisplay, style: const TextStyle(color: Colors.white, fontSize: 15, fontWeight: FontWeight.w700), overflow: TextOverflow.ellipsis),
          const SizedBox(height: 2),
          Text('Técnico $brand', style: const TextStyle(color: Color(0xCCFFFFFF), fontSize: 11), overflow: TextOverflow.ellipsis),
          const SizedBox(height: 6),
          Row(children: [
            Container(width: 8, height: 8, decoration: const BoxDecoration(color: Color(0xff7CFF9C), shape: BoxShape.circle, boxShadow: [BoxShadow(color: Color(0x807CFF9C), blurRadius: 6)])),
            const SizedBox(width: 6),
            const Text('Em atendimento', style: TextStyle(color: Colors.white, fontSize: 11, fontWeight: FontWeight.w600)),
          ]),
          const SizedBox(height: 2),
          Text(_fmtElapsed(), style: const TextStyle(color: Color(0xEEFFFFFF), fontSize: 13, fontWeight: FontWeight.w800, fontFeatures: [FontFeature.tabularFigures()])),
        ],
      );
    }
    return Row(
      children: [
        left,
        const SizedBox(width: 12),
        Expanded(child: info),
        Column(
          mainAxisAlignment: MainAxisAlignment.spaceBetween,
          children: [
            _collapseBtn(),
            _brandLogo(30),
          ],
        ),
      ],
    );
  }

  // Avatar de um técnico arbitrário (multi). Foto se houver, senão inicial.
  Widget _avatarFor(String name, String photoPath, double size) {
    final f = photoPath.isNotEmpty ? File(photoPath) : null;
    final hasPhoto = f != null && f.existsSync();
    final initial = name.isNotEmpty ? name.trim()[0].toUpperCase() : '?';
    return Container(
      width: size, height: size,
      decoration: BoxDecoration(
        color: const Color(0xff0A6A3A),
        shape: BoxShape.circle,
        border: Border.all(color: Colors.white, width: 2),
      ),
      clipBehavior: Clip.antiAlias,
      alignment: Alignment.center,
      child: hasPhoto
          ? Image.file(f!, key: ValueKey('$photoPath$_photoTag'), fit: BoxFit.cover, width: size, height: size, gaplessPlayback: true,
              errorBuilder: (_, __, ___) => Text(initial, style: TextStyle(color: Colors.white, fontSize: size * 0.45, fontWeight: FontWeight.w800)))
          : Text(initial, style: TextStyle(color: Colors.white, fontSize: size * 0.45, fontWeight: FontWeight.w800)),
    );
  }

  // Recolhido: bolinha verde mínima (janela ~48x48 toda verde + ponto branco pulsante no centro).
  // Discreta, não atrapalha a tela. Clica pra expandir.
  Widget _collapsedBody(String brand) {
    return GestureDetector(
      onTap: () => _setCollapsed(false),
      child: Container(
        color: const Color(0xff01A862),
        alignment: Alignment.center,
        child: Container(
          width: 14, height: 14,
          decoration: BoxDecoration(
            color: Colors.white,
            shape: BoxShape.circle,
            boxShadow: [BoxShadow(color: Colors.white.withOpacity(0.6), blurRadius: 6)],
          ),
        ),
      ),
    );
  }

  Widget _collapseBtn() {
    return InkWell(
      onTap: () => _setCollapsed(true),
      borderRadius: BorderRadius.circular(8),
      child: Container(
        width: 22, height: 22,
        alignment: Alignment.center,
        child: const Icon(Icons.unfold_less, color: Color(0xCCFFFFFF), size: 16),
      ),
    );
  }

  // Redimensiona/reposiciona a própria sub-window: recolhido = bolinha 72x72 colada à direita;
  // expandido = card 320x140. Usa WindowController.fromWindowId (NÃO o windowManager singleton).
  Future<void> _setCollapsed(bool collapse) async {
    setState(() => _collapsed = collapse);
    try {
      final screens = await window_size.getScreenList();
      final primary = screens.isNotEmpty ? screens.first : null;
      final ctrl = WindowController.fromWindowId(widget.windowId);
      if (primary != null) {
        final frame = primary.visibleFrame;
        if (collapse) {
          const s = 48.0;
          await ctrl.setFrame(Rect.fromLTWH(frame.right - s - 14, frame.bottom - s - 14, s, s));
        } else {
          await ctrl.setFrame(Rect.fromLTWH(frame.right - 320 - 16, frame.bottom - 140 - 16, 320, 140));
        }
      }
    } catch (_) {}
  }

  Widget _techAvatar(double size) {
    final f = _techPhotoPath.isNotEmpty ? File(_techPhotoPath) : null;
    final hasPhoto = f != null && f.existsSync();
    final initial = _techName.isNotEmpty ? _techName.trim()[0].toUpperCase() : '?';
    return Container(
      width: size, height: size,
      decoration: BoxDecoration(
        color: Colors.white.withOpacity(0.15),
        shape: BoxShape.circle,
        border: Border.all(color: Colors.white, width: 2),
      ),
      clipBehavior: Clip.antiAlias,
      alignment: Alignment.center,
      child: hasPhoto
          ? Image.file(f!, key: ValueKey(_photoTag), fit: BoxFit.cover, width: size, height: size,
              gaplessPlayback: true,
              errorBuilder: (_, __, ___) => Text(initial, style: TextStyle(color: Colors.white, fontSize: size * 0.45, fontWeight: FontWeight.w800)))
          : Text(initial, style: TextStyle(color: Colors.white, fontSize: size * 0.45, fontWeight: FontWeight.w800)),
    );
  }

  Widget _brandLogo(double size) {
    final f = _brandLogoPath.isNotEmpty ? File(_brandLogoPath) : null;
    final hasLogo = f != null && f.existsSync();
    return Container(
      width: size, height: size,
      padding: const EdgeInsets.all(6),
      decoration: BoxDecoration(
        color: Colors.white,
        borderRadius: BorderRadius.circular(10),
      ),
      child: hasLogo
          ? Image.file(f!, fit: BoxFit.contain)
          : Image.asset('assets/logo.png', fit: BoxFit.contain,
              errorBuilder: (_, __, ___) => const Icon(Icons.shield, color: Color(0xff01A862), size: 28)),
    );
  }

  Widget _closeBtn() {
    return InkWell(
      onTap: () async { try { await windowManager.close(); } catch (_) {} },
      borderRadius: BorderRadius.circular(10),
      child: Container(
        width: 24, height: 24,
        alignment: Alignment.center,
        child: const Icon(Icons.close, color: Color(0xCCFFFFFF), size: 16),
      ),
    );
  }
}
