import 'package:flutter/material.dart';
import 'package:mobile_scanner/mobile_scanner.dart';
import '../services/text_sender.dart';

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

  /// 上一次已发起配对的二维码原文。
  ///
  /// 配对失败后扫描会立即恢复，而二维码往往还停在取景框里，不加去重会在
  /// 数秒内对同一张码发起多次请求——5 次 bad_code 就会触发 PC 端 60 秒冷却，
  /// 用户只扫了一次却被关在门外一分钟。PC 端续期后二维码内容必然变化，
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
          // 扫描器
          if (_scanning)
            MobileScanner(
              controller: MobileScannerController(
                detectionSpeed: DetectionSpeed.noDuplicates,
                facing: CameraFacing.back,
              ),
              onDetect: _onDetect,
            ),

          // 扫描框
          Center(
            child: Container(
              width: 250,
              height: 250,
              decoration: BoxDecoration(
                border: Border.all(color: Colors.white54, width: 2),
                borderRadius: BorderRadius.circular(12),
              ),
            ),
          ),

          // 提示文字
          Positioned(
            bottom: 80,
            left: 0,
            right: 0,
            child: Text(
              _message,
              textAlign: TextAlign.center,
              style: TextStyle(color: _messageColor, fontSize: 16),
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

    // 检查 QR 格式
    if (!raw.startsWith('beetype://')) {
      setState(() {
        _message = '无效的二维码，请扫描 BeeType 二维码';
        _messageColor = Colors.red;
      });
      return;
    }

    // 同一张二维码只尝试一次，杜绝取景框不挪开时的请求风暴
    if (raw == _lastAttemptedRaw) return;
    _lastAttemptedRaw = raw;

    setState(() {
      _scanning = false;
      _message = '正在配对...';
      _messageColor = Colors.orange;
    });

    try {
      final result = await widget.sender.pairFromQr(raw);
      if (result.ok) {
        setState(() {
          _message = '连接成功！';
          _messageColor = Colors.green;
        });
        await Future.delayed(const Duration(milliseconds: 800));
        if (mounted) {
          Navigator.of(context).pop(true);
        }
      } else {
        setState(() {
          _message = '${result.error ?? '配对失败，请重试'}\n'
              '请重新打开电脑端配对窗口获取新二维码，再对准扫描';
          _messageColor = Colors.red;
          _scanning = true;
        });
      }
    } catch (e) {
      setState(() {
        _message = '连接失败：$e';
        _messageColor = Colors.red;
        _scanning = true;
      });
    }
  }
}
