import 'package:flutter/material.dart';
import 'package:flutter_hbb/common.dart';
import 'package:flutter_hbb/desktop/widgets/tabbar_widget.dart';
import 'package:flutter_hbb/models/platform_model.dart';
import 'package:flutter_hbb/models/state_model.dart';
import 'package:get/get.dart';
import 'package:window_manager/window_manager.dart';

class InstallPage extends StatefulWidget {
  const InstallPage({Key? key}) : super(key: key);

  @override
  State<InstallPage> createState() => _InstallPageState();
}

class _InstallPageState extends State<InstallPage> {
  final tabController = DesktopTabController(tabType: DesktopTabType.main);

  _InstallPageState() {
    Get.put<DesktopTabController>(tabController);
    const label = "install";
    tabController.add(TabInfo(
        key: label,
        label: label,
        closable: false,
        page: _InstallPageBody(
          key: const ValueKey(label),
        )));
  }

  @override
  void dispose() {
    super.dispose();
    Get.delete<DesktopTabController>();
  }

  @override
  Widget build(BuildContext context) {
    return DragToResizeArea(
      resizeEdgeSize: stateGlobal.resizeEdgeSize.value,
      enableResizeEdges: windowManagerEnableResizeEdges,
      child: Container(
        child: Scaffold(
            backgroundColor: Theme.of(context).colorScheme.background,
            body: DesktopTab(controller: tabController)),
      ),
    );
  }
}

class _InstallPageBody extends StatefulWidget {
  const _InstallPageBody({Key? key}) : super(key: key);

  @override
  State<_InstallPageBody> createState() => _InstallPageBodyState();
}

class _InstallPageBodyState extends State<_InstallPageBody>
    with WindowListener {
  late final TextEditingController controller;
  final RxBool showProgress = false.obs;
  final RxBool btnEnabled = true.obs;
  // ConectDesk: tela de conclusão pós-instalação (✅ + ID de conexão).
  final RxBool showDone = false.obs;
  final RxString myId = ''.obs;

  _InstallPageBodyState() {
    controller = TextEditingController(text: bind.installInstallPath());
  }

  @override
  void initState() {
    windowManager.addListener(this);
    super.initState();
  }

  @override
  void dispose() {
    windowManager.removeListener(this);
    super.dispose();
  }

  @override
  void onWindowClose() {
    gFFI.close();
    super.onWindowClose();
    windowManager.setPreventClose(false);
    windowManager.close();
  }

  // ConectDesk: instalador profissional pra cliente leigo (1ª instalação, sem
  // rede de segurança). Tela única branded, 1 botão, progresso e conclusão com
  // o ID — sem opções/caminho/agreement RustDesk que confundem o usuário.
  static const Color _green = Color(0xff01A862);
  static const Color _greenDeep = Color(0xff0A6A3A);

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      backgroundColor: Colors.white,
      body: Center(
        child: Obx(() => showDone.value ? _doneView() : _installView()),
      ),
    );
  }

  Widget _logo(double size) => Container(
        width: size,
        height: size,
        decoration: const BoxDecoration(
          gradient: LinearGradient(
              begin: Alignment.topLeft,
              end: Alignment.bottomRight,
              colors: [_greenDeep, _green]),
          shape: BoxShape.circle,
        ),
        child: Icon(Icons.support_agent_rounded,
            color: Colors.white, size: size * 0.55),
      );

  Widget _installView() {
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 48),
      child: Column(
        mainAxisAlignment: MainAxisAlignment.center,
        children: [
          _logo(88),
          const SizedBox(height: 22),
          const Text('ConectDesk',
              style: TextStyle(
                  fontSize: 26,
                  fontWeight: FontWeight.w800,
                  color: Color(0xff1A1A2E))),
          const SizedBox(height: 4),
          const Text('Suporte remoto',
              style: TextStyle(fontSize: 14, color: Color(0xff7A7A8C))),
          const SizedBox(height: 22),
          const Text(
            'Clique no botão abaixo para instalar.\nLeva alguns segundos e não precisa configurar nada.',
            textAlign: TextAlign.center,
            style: TextStyle(
                fontSize: 14, color: Color(0xff4A4A5A), height: 1.4),
          ),
          const SizedBox(height: 32),
          Obx(() => showProgress.value
              ? Column(children: const [
                  SizedBox(
                      width: 34,
                      height: 34,
                      child: CircularProgressIndicator(
                          strokeWidth: 3,
                          valueColor:
                              AlwaysStoppedAnimation<Color>(_green))),
                  SizedBox(height: 14),
                  Text('Instalando ConectDesk...',
                      style: TextStyle(
                          fontSize: 14,
                          fontWeight: FontWeight.w600,
                          color: _greenDeep)),
                ])
              : SizedBox(
                  width: 280,
                  height: 52,
                  child: ElevatedButton.icon(
                    icon: const Icon(Icons.download_rounded, size: 20),
                    label: const Text('Instalar ConectDesk',
                        style: TextStyle(
                            fontSize: 16, fontWeight: FontWeight.w700)),
                    style: ElevatedButton.styleFrom(
                      backgroundColor: _green,
                      foregroundColor: Colors.white,
                      shape: RoundedRectangleBorder(
                          borderRadius: BorderRadius.circular(12)),
                    ),
                    onPressed: btnEnabled.value ? install : null,
                  ),
                )),
        ],
      ),
    );
  }

  Widget _doneView() {
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 48),
      child: Column(
        mainAxisAlignment: MainAxisAlignment.center,
        children: [
          const Icon(Icons.check_circle_rounded, color: _green, size: 72),
          const SizedBox(height: 18),
          const Text('ConectDesk instalado!',
              style: TextStyle(
                  fontSize: 22,
                  fontWeight: FontWeight.w800,
                  color: Color(0xff1A1A2E))),
          const SizedBox(height: 22),
          Obx(() => myId.value.isEmpty
              ? const SizedBox.shrink()
              : Column(children: [
                  const Text('ID de conexão',
                      style:
                          TextStyle(fontSize: 12, color: Color(0xff7A7A8C))),
                  const SizedBox(height: 6),
                  Container(
                    padding: const EdgeInsets.symmetric(
                        horizontal: 18, vertical: 10),
                    decoration: BoxDecoration(
                        color: const Color(0xffF0FAF5),
                        borderRadius: BorderRadius.circular(10),
                        border: Border.all(
                            color: const Color(0xffBDEBD4))),
                    child: Text(myId.value,
                        style: const TextStyle(
                            fontSize: 22,
                            fontWeight: FontWeight.w800,
                            letterSpacing: 1.5,
                            color: _greenDeep)),
                  ),
                ])),
          const SizedBox(height: 24),
          const Text('Já pode fechar esta janela.',
              style: TextStyle(fontSize: 14, color: Color(0xff4A4A5A))),
          const SizedBox(height: 22),
          SizedBox(
            width: 180,
            height: 46,
            child: OutlinedButton(
              style: OutlinedButton.styleFrom(
                foregroundColor: _greenDeep,
                side: const BorderSide(color: _green),
                shape: RoundedRectangleBorder(
                    borderRadius: BorderRadius.circular(12)),
              ),
              onPressed: () => windowManager.close(),
              child: const Text('Fechar',
                  style:
                      TextStyle(fontSize: 15, fontWeight: FontWeight.w700)),
            ),
          ),
        ],
      ),
    );
  }

  void install() async {
    btnEnabled.value = false;
    showProgress.value = true;
    // Opções padrão (atalhos), sem expor escolhas ao usuário. Path default.
    String args = ' startmenu desktopicon';
    await bind.installInstallMe(options: args, path: controller.text);
    // Se o processo não foi reiniciado pelo core, mostra a conclusão com o ID.
    // (Se reiniciar antes, o serviço já ficou instalado de qualquer forma.)
    try {
      myId.value = await bind.mainGetMyId();
    } catch (_) {}
    showProgress.value = false;
    showDone.value = true;
  }
}
