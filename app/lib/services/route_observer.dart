import 'package:flutter/widgets.dart';

/// 全局路由观察者：首页靠它拿到"子页面退场、我重新回到前台"这个**事件**。
///
/// 为什么不用"pushNamed 的 future 返回"作为时机：**手势返回时，pop 的 future 在
/// 转场动画刚开始就完成了**（按钮返回则是在动画结束时），那时首页路由还是"被覆盖"
/// 状态，请求聚焦会被**静默拒绝**（canRequestFocus=false，用户实测的"键盘弹了但
/// 光标不在"）。见 `home_page.dart` 的 didPopNext / _requestKeyboard。
final RouteObserver<ModalRoute<void>> appRouteObserver =
    RouteObserver<ModalRoute<void>>();
