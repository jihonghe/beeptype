import 'package:flutter_test/flutter_test.dart';
import 'package:beeptype/services/tcp_service.dart';

void main() {
  group('TcpMessage 协议编解码', () {
    test('text 消息编码为 JSON 行（以 \\n 结尾）', () {
      final msg = TcpMessage('text', {
        'id': 'abc-1',
        'text': '你好，BeepType',
        'ts': 1726000000000,
      });

      final line = msg.encode();
      expect(line.endsWith('\n'), isTrue);
      expect(line, contains('"type":"text"'));
      expect(line, contains('你好'));
    });

    test('ack 消息往返解码保留全部字段', () {
      const raw =
          '{"type":"ack","id":"abc-1","ok":true,"pasted":true,'
          '"srv_recv_ms":12}\n';
      final msg = TcpMessage.decode(raw);

      expect(msg, isNotNull);
      expect(msg!.type, 'ack');
      expect(msg.data['id'], 'abc-1');
      expect(msg.data['ok'], true);
      expect(msg.data['pasted'], true);
      expect(msg.data['srv_recv_ms'], 12);
    });

    test('畸形行解码返回 null 而不是抛异常', () {
      expect(TcpMessage.decode('not a json\n'), isNull);
      expect(TcpMessage.decode('{"no_type":true}\n'), isNull);
    });
  });
}
