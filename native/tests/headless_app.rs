use nsh_engine::{EngineConfig, Points, Root, Size};
use nsh_testing::Headless;

#[test]
fn mounts_workbench_and_executes_command_bar() -> Result<(), String> {
    let mut app = Headless::start(EngineConfig {
        viewport: Size::new(Points(1280.0), Points(820.0)),
        ..EngineConfig::new(Root::of::<nsh_app_oprobe::App>())
    })
    .map_err(|e| e.to_string())?;

    app.wait_for("oprobe · OpenDAL Storage Stack Profiler")?;
    app.press("最优 Operator 代码")?;
    app.wait_for("Operator::via_iter")?;
    app.press("多云 FinOps 账单")?;
    app.wait_for("AWS S3 Standard")?;
    Ok(())
}
