//! `nsh-app-oprobe` — Native Shell (NSH v0.6) 双端视图壳层：
//! 复用根包 `oprobe` 的全部存储遥测、网络握手、多云成本、自动寻优与报告导出内核，
//! 一套纯 Rust 组件同时渲染至 `--target gpui` 桌面窗口与 `--target tui` 终端字符网格。

pub mod store;
pub mod ui;

use nsh_base::theme;
use nsh_core::prelude::*;
use store::ProfilerStore;

#[native_module("oprobe/native")]
fn native(registry: &mut Registry) {
    registry.register::<App>();
}

#[component]
pub fn App() -> impl IntoElement {
    let store = ProfilerStore::new();
    provide_context(store);

    view()
        .flex_1()
        .bg(move || theme().background)
        .text_color(move || theme().foreground)
        .p_safe_area()
        .on_key(" ", move || store.toggle_probe_session())
        .on_key("o", move || store.start_autotune_sweep())
        .on_key("e", move || store.export_snapshot("oprobe_nsh_report"))
        .on_key("Escape", move || store.cancel_active_task())
        .child(ui::workbench_screen(store))
}
