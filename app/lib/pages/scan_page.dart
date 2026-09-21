import 'dart:async';

import 'package:flutter/material.dart';
import 'package:mobile_scanner/mobile_scanner.dart';
import '../services/text_sender.dart';

/// 扫描框里那条来回扫动的光带（微信扫一扫的观感）
class _ScanLine extends StatefulWidget {
  const _ScanLine();

  @override
  State<_ScanLine> createState() => _ScanLineState();
}

class _ScanLineState extends State<_ScanLine>
    with SingleTickerProviderStateMixin {
  late final AnimationController _c = AnimationController(
    vsync: this,
    duration: const Duration(milliseconds: 1800),
  )..repeat(reverse: true);

  @override
  void dispose() {
    _c.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return AnimatedBuilder(
      animation: _c,
      builder: (context, _) => Align(
        alignment: Alignment(0, _c.value * 2 - 1),
        child: Container(
          height: 2,
          margin: const EdgeInsets.symmetric(horizontal: 10),
          decoration: const BoxDecoration(
            gradient: LinearGradient(
              colors: [
                Color(0x004fc3f7),
                Color(0xff4fc3f7),
                Color(0x004fc3f7),
              ],
            ),
          ),
        ),
      ),
    );
  }
}

class ScanPage extends StatefulWidget {
  final TextSender sender;

  const ScanPage({super.key, required this.sender});

  @override
  State<ScanPage> createState() => _ScanPageState();
}

class _ScanPageState extends State<ScanPage> {
  bool _scanning = true;
  String _message = '扫描 PC 端显示的二维码';
  Color _messageColor = Colors.white;

  /// 失败提示的自动让位计时器：1.5 秒后重新武装扫描，不用手点按钮
  Timer? _rearmTimer;

  /// 上一次失败后是否已经重新武装过（允许同一张码再试一次）
  bool _rearmedOnce = false;

  /// 扫描器控制器：**必须常驻**。
  ///
  /// 原先它在 build() 里现造、且 `_scanning` 为 false 时连控件一起移除——
  /// 一次配对失败就等于把相机拆掉再重建，插件常常回不到可用状态，用户只能
  /// 退出页面重新进（"不能持续扫描"的真身）。去重改由 [_lastAttemptedRaw]
  /// 自己管，这样「重新扫描」才真的能让同一张码再试一次。
  late final MobileScannerController _controller = MobileScannerController(
    detectionSpeed: DetectionSpeed.normal,
    facing: CameraFacing.back,
  );

  @override
  void dispose() {
    _rearmTimer?.cancel();
    _controller.dispose();
    super.dispose();
  }

  /// 失败后自动重新武装：清掉"同一张码只试一次"的去重，并复位提示。
  /// 提示只在**真正发起新一轮配对**时被替换（那时会显示「正在配对…」），
  /// 所以不会出现"提示还挂着、其实什么都没发生"的误导。
  void _rearmScan() {
    setState(() {
      _scanning = true;
      _rearmedOnce = true;
      _message = '扫描 PC 端显示的二维码';
      _messageColor = Colors.white;
    });
  }

  /// 上一次已发起配对的二维码原文。
  ///
  /// 配对失败后扫描会立即恢复，而二维码往往还停在取景框里，不加去重会在
  /// 数秒内对同一张码反复发请求。PC 端续期后二维码内容必然变化，
  /// 届时同位置的新码自然允许再次发起。
  String? _lastAttemptedRaw;

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      backgroundColor: Colors.black,
      appBar: AppBar(
        title: const Text('扫码连接电脑'),
        backgroundColor: Colors.transparent,
        foregroundColor: Colors.white,
      ),
      body: Stack(
        children: [
          // 扫描器：常驻不拆装（拆一次就得重新初始化相机，失败后往往回不来）
          MobileScanner(controller: _controller, onDetect: _onDetect),

          // 扫描框 + 扫动光带
          Center(
            child: Container(
              width: 250,
              height: 250,
              clipBehavior: Clip.antiAlias,
              decoration: BoxDecoration(
                border: Border.all(color: Colors.white54, width: 2),
                borderRadius: BorderRadius.circular(12),
              ),
              child: const _ScanLine(),
            ),
          ),

          // 提示文字
          Positioned(
            bottom: 80,
            left: 0,
            right: 0,
            child: Column(
              mainAxisSize: MainAxisSize.min,
              children: [
                Text(
                  _message,
                  textAlign: TextAlign.center,
                  style: TextStyle(color: _messageColor, fontSize: 16),
                ),
              ],
            ),
          ),
        ],
      ),
    );
  }

  void _onDetect(BarcodeCapture capture) async {
    if (!_scanning) return;

    final barcodes = capture.barcodes;
    if (barcodes.isEmpty) return;

    final raw = barcodes.first.rawValue;
    if (raw == null) return;

    // 不是我们的二维码格式：这一帧没读懂而已（二维码在电脑端每约 3 分钟自动换新，
    // 扫描瞬间也可能糊），静默忽略、继续扫——下一帧通常就对了。
    // 把它当失败报成红字只会吓到用户，而且它本来就不影响最终结果。
    if (!raw.startsWith('beeptype://')) {
      debugPrint('VL_SCAN 忽略一次未读懂的内容（${raw.length} 字节）');
      return;
    }

    // 同一张码在"上一次尝试还没结束"时不重复发起（避免同一帧连发）：
    // 失败后 1.5 秒会自动重新武装，届时同一张码也可以再试——扫码页不设次数上限
    if (!_scanning) return;
    if (raw == _lastAttemptedRaw && !_rearmedOnce) return;
    _lastAttemptedRaw = raw;
    _rearmedOnce = false;
    _rearmTimer?.cancel();

    setState(() {
      _scanning = false;
      _message = '正在配对...';
      _messageColor = Colors.orange;
    });

    try {
      final result = await widget.sender.pairFromQr(raw);
      if (result.ok) {
        // 直接跳回：跳转本身就是"配对成功"的反馈，不需要先显示一句再跳
        if (mounted) {
          Navigator.of(context).pop(true);
        }
        return;
      } else {
        setState(() {
          _message = result.error ?? '配对失败，请重试';
          _messageColor = Colors.red;
          _scanning = true;
        });
        // 1.5 秒后自动让位：提示收掉、扫描重新武装（同一张码也可以再试，全程无需手动）
        _rearmTimer?.cancel();
        _rearmTimer = Timer(const Duration(milliseconds: 1500), () {
          if (mounted) _rearmScan();
        });
      }
    } catch (e) {
      setState(() {
        _message = '连接失败：$e';
        _messageColor = Colors.red;
        _scanning = true;
      });
      _rearmTimer?.cancel();
      _rearmTimer = Timer(const Duration(milliseconds: 1500), () {
        if (mounted) _rearmScan();
      });
    }
  }
}
