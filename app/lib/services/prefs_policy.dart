/// 两个行为开关（「自动粘贴」「自动发送」）的联动规则（纯逻辑，便于单测）。
///
/// 用户定稿：这两个开关在语义上**是一对**，同步时永远**整对**发出去——只发被改动的
/// 那一个会留下"依赖不一致"的窗口（例如先关掉粘贴、再单独打开发送，电脑端只能靠
/// 猜另一半）。规则与电脑端完全一致：
///
/// **自动粘贴关 → 自动发送同时关**（没东西可粘贴，就没有可发送的）。
/// 「电脑端设置」这一块该不该显示（纯逻辑，便于单测）。
///
/// 只有**当前这条连接是常用设备身份**、且已经从电脑端同步到值时才显示：
/// 临时访客不显示（电脑端本来也不接受它的修改）；还没同步到值时也不显示，
/// 免得先闪一下错的状态。
bool showPcSettings({
  required bool hasPrefs,
  required bool? sessionOk,
  required bool? sessionIsPrimary,
}) =>
    hasPrefs && (sessionOk ?? false) && (sessionIsPrimary ?? false);

({bool autoPaste, bool autoSend}) prefsAfterToggle({
  required bool autoPaste,
  required bool autoSend,
  bool? setPaste,
  bool? setSend,
}) {
  var paste = setPaste ?? autoPaste;
  var send = setSend ?? autoSend;
  if (!paste) send = false;
  return (autoPaste: paste, autoSend: send);
}
