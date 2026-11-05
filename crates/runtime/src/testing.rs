//! 测试要用到真实窗口的那部分：像系统那样交来打开请求。没有窗口的测试工具在 `aureole-testing` 里

use crate::open::OpenRequest;

/// 测试用：像系统或再次启动那样交来一个打开请求，和真的走同一条路：排进队列、唤醒事件循环、
/// 主窗口建好后交给 [`crate::App::on_open`]。只在开了 `on_open` 或单实例的应用里有效
pub fn simulate_open(request: OpenRequest) {
    crate::open::push(request);
}
