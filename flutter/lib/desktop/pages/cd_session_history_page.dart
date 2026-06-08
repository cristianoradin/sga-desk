// Histórico de sessões ConectDesk dessa máquina. Lê option cd_session_history (JSON array
// salvo pelo agent via sync_session_history a cada 5min). Mostra técnico, data, duração,
// motivo. Sem dependência HTTP direta — Flutter não tem token agent.
import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter_hbb/models/platform_model.dart';

class CdSessionHistoryPage extends StatefulWidget {
  const CdSessionHistoryPage({Key? key}) : super(key: key);
  @override
  State<CdSessionHistoryPage> createState() => _CdSessionHistoryPageState();
}

class _CdSessionHistoryPageState extends State<CdSessionHistoryPage> {
  List<Map<String, dynamic>> _sessions = [];

  @override
  void initState() {
    super.initState();
    _load();
  }

  void _load() {
    final raw = bind.mainGetOptionSync(key: 'cd_session_history');
    if (raw.isEmpty) {
      setState(() => _sessions = []);
      return;
    }
    try {
      final parsed = jsonDecode(raw);
      if (parsed is List) {
        setState(() => _sessions = parsed.cast<Map<String, dynamic>>());
      }
    } catch (_) {
      setState(() => _sessions = []);
    }
  }

  String _formatDate(dynamic v) {
    if (v == null) return '—';
    DateTime? dt;
    if (v is int) {
      dt = DateTime.fromMillisecondsSinceEpoch(v);
    } else if (v is String) {
      dt = DateTime.tryParse(v);
    }
    if (dt == null) return '—';
    final l = dt.toLocal();
    String pad(int n) => n.toString().padLeft(2, '0');
    return '${pad(l.day)}/${pad(l.month)}/${l.year} ${pad(l.hour)}:${pad(l.minute)}';
  }

  String _duration(dynamic start, dynamic end) {
    DateTime? s, e;
    if (start is int) s = DateTime.fromMillisecondsSinceEpoch(start);
    else if (start is String) s = DateTime.tryParse(start);
    if (end is int) e = DateTime.fromMillisecondsSinceEpoch(end);
    else if (end is String) e = DateTime.tryParse(end);
    if (s == null) return '—';
    final endTime = e ?? DateTime.now();
    final d = endTime.difference(s);
    if (d.inHours > 0) return '${d.inHours}h ${d.inMinutes.remainder(60)}m';
    if (d.inMinutes > 0) return '${d.inMinutes}m ${d.inSeconds.remainder(60)}s';
    return '${d.inSeconds}s';
  }

  @override
  Widget build(BuildContext context) {
    final primary = const Color(0xff01A862);
    return Scaffold(
      appBar: AppBar(
        title: const Text('Histórico de Sessões'),
        backgroundColor: primary,
        foregroundColor: Colors.white,
        actions: [
          IconButton(icon: const Icon(Icons.refresh), onPressed: _load, tooltip: 'Recarregar'),
        ],
      ),
      body: _sessions.isEmpty
          ? Center(
              child: Column(mainAxisAlignment: MainAxisAlignment.center, children: [
                Icon(Icons.history, size: 64, color: Colors.grey[400]),
                const SizedBox(height: 12),
                Text('Sem sessões registradas ainda.', style: TextStyle(color: Colors.grey[600], fontSize: 16)),
                const SizedBox(height: 4),
                Text('O histórico atualiza a cada 5 minutos.', style: TextStyle(color: Colors.grey[500], fontSize: 12)),
              ]),
            )
          : ListView.separated(
              padding: const EdgeInsets.all(12),
              itemCount: _sessions.length,
              separatorBuilder: (_, __) => const Divider(height: 1),
              itemBuilder: (ctx, i) {
                final s = _sessions[i];
                final tech = (s['technician'] as String?) ?? 'Técnico';
                final reason = (s['reason'] as String?) ?? '';
                final created = s['created_at'];
                final ended = s['ended_at'];
                final ongoing = ended == null;
                return ListTile(
                  leading: CircleAvatar(
                    backgroundColor: ongoing ? primary : Colors.grey[400],
                    child: Text(tech.isNotEmpty ? tech[0].toUpperCase() : '?', style: const TextStyle(color: Colors.white, fontWeight: FontWeight.w700)),
                  ),
                  title: Text(tech, style: const TextStyle(fontWeight: FontWeight.w600)),
                  subtitle: Column(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      Text(_formatDate(created), style: const TextStyle(fontSize: 12)),
                      if (reason.isNotEmpty) Text(reason, style: TextStyle(fontSize: 11, color: Colors.grey[600]), maxLines: 2, overflow: TextOverflow.ellipsis),
                    ],
                  ),
                  trailing: Column(
                    mainAxisAlignment: MainAxisAlignment.center,
                    crossAxisAlignment: CrossAxisAlignment.end,
                    children: [
                      Text(_duration(created, ended), style: TextStyle(fontSize: 13, fontWeight: FontWeight.w700, color: ongoing ? primary : Colors.grey[700])),
                      if (ongoing) Text('em curso', style: TextStyle(fontSize: 10, color: primary, fontWeight: FontWeight.w600)),
                    ],
                  ),
                );
              },
            ),
    );
  }
}
