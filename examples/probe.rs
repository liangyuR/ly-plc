use std::sync::Arc;
use std::time::Duration;

use ly_plc::{EventSink, Logbook, PlcConfig, PlcEngine, PlcEvent};

#[tokio::main]
async fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .ok_or("用法: probe <config.json> [点位ID=值 ...]")?;
    let config = PlcConfig::load(path.as_ref()).ok_or("配置文件读取失败")?;
    config.validate()?;

    let sink: EventSink = Arc::new(|e| match e {
        PlcEvent::Logs(logs) => logs.iter().for_each(|l| {
            println!(
                "[{}] {} {}",
                l.category,
                l.point_name.as_deref().unwrap_or("-"),
                l.message
            )
        }),
        PlcEvent::Edge(e) => println!("[edge] {} rising={}", e.point_name, e.rising),
        _ => {}
    });
    let db = std::env::temp_dir().join(format!("ly-plc-probe-{}.db", std::process::id()));
    let logbook = Arc::new(Logbook::open(&db, sink.clone(), 1)?);
    let engine = PlcEngine::new(config, logbook, sink);
    engine.connect().await;
    tokio::time::sleep(Duration::from_millis(800)).await;

    for arg in args {
        let (id, raw) = arg.split_once('=').ok_or("写入参数格式: 点位ID=值")?;
        let value = serde_json::from_str(raw).unwrap_or(serde_json::Value::String(raw.into()));
        if let Err(e) = engine.write_point(id, &value).await {
            println!("写入 {id} 失败: {e}");
        }
    }
    tokio::time::sleep(Duration::from_millis(800)).await;

    let status = engine.status();
    println!("状态: {:?} {}", status.state, status.message);
    let values = engine.values();
    for p in engine.config().points {
        println!(
            "{:<12} {:<14} = {}",
            p.id,
            p.address,
            serde_json::to_string(&values.get(&p.id)).unwrap()
        );
    }
    engine.disconnect().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = std::fs::remove_file(&db);
    Ok(())
}
