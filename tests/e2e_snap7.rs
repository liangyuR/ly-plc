//! 端到端：引擎对着 `tools/s7_server.py`（python-snap7 的服务端）跑本次新增的能力。
//!
//! 默认忽略，需要装了 python-snap7 的 python（`tools/requirements.txt`）：
//!
//! ```text
//! python -m venv .venv && .venv/Scripts/pip install "python-snap7>=3.2"
//! LY_PLC_S7_PYTHON=.venv/Scripts/python.exe cargo test --test e2e_snap7 -- --ignored --nocapture
//! ```

mod support;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use ly_plc::{DataType, LinkState, PlcError, PlcValue, VerifyOptions};
use serde_json::json;
use support::{config, point, rig, wait_until};

struct Snap7 {
    python: String,
    port: u16,
    child: Option<Child>,
}

impl Snap7 {
    fn start(python: &str) -> Self {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut s = Self {
            python: python.into(),
            port,
            child: None,
        };
        s.spawn();
        s
    }

    fn spawn(&mut self) {
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tools/s7_server.py");
        let child = Command::new(&self.python)
            .args([script, "--port", &self.port.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start tools/s7_server.py");
        self.child = Some(child);
        let started = Instant::now();
        while std::net::TcpStream::connect(("127.0.0.1", self.port)).is_err() {
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "snap7 server did not start"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for Snap7 {
    fn drop(&mut self) {
        self.kill();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs python-snap7; see the module docs"]
async fn new_capabilities_against_python_snap7() {
    let Ok(python) = std::env::var("LY_PLC_S7_PYTHON") else {
        panic!("set LY_PLC_S7_PYTHON to a python with python-snap7 installed");
    };
    let mut server = Snap7::start(&python);
    let mut mirror = point("mirror", "DB2.DBB0.42", DataType::Bytes);
    mirror.poll = false;
    let rig = rig(config(
        server.port,
        50,
        vec![
            // tools/s7_server.py 预置 DB1.DBW4 = -7。
            point("i16", "DB1.DBW4", DataType::I16),
            point("result", "DB1.DBB40", DataType::U8),
            point("raw", "DB1.DBB60.42", DataType::Bytes),
            point("missing", "DB9.DBW0", DataType::U16),
            mirror,
        ],
    ));
    rig.start().await;
    let status = rig.engine.status();
    assert_eq!((status.dials, status.outage_dials), (1, 1));
    let values = rig.engine.values();
    assert_eq!(values["i16"].value, Some(PlcValue::Int(-7)));
    assert!(matches!(
        values["missing"].error,
        Some(PlcError::Rejected(_))
    ));
    assert!(!values.contains_key("mirror"));

    // 1. 写后确认：写 → 5 ms → 立即读。
    let started = Instant::now();
    let report = rig
        .engine
        .write_point_verified(
            "result",
            &json!(2),
            VerifyOptions {
                attempts: 12,
                delay: Duration::from_millis(5),
            },
        )
        .await
        .unwrap();
    let took = started.elapsed();
    println!("write_point_verified: {took:?}, {report:?}");
    assert_eq!(report.attempts, 1);
    assert!(took < Duration::from_millis(100), "{took:?}");
    assert_eq!(rig.engine.read_now("result").await, Ok(PlcValue::Int(2)));

    // 2 + 3. 只写的原始字节点位：写 STRING 头加内容，按需读回原样。
    let mut bytes = vec![40u8, 5];
    bytes.extend_from_slice(b"VIN42");
    bytes.resize(42, 0);
    rig.engine
        .write_point("mirror", &json!(bytes))
        .await
        .unwrap();
    assert_eq!(
        rig.engine.read_now("mirror").await,
        Ok(PlcValue::Bytes(bytes))
    );
    assert!(!rig.engine.values().contains_key("mirror"));
    let PlcValue::Bytes(raw) = rig.engine.read_now("raw").await.unwrap() else {
        panic!("raw is a Bytes point");
    };
    assert_eq!(raw.len(), 42);

    // 4. 被拒绝 vs 链路错误。
    let e = rig.engine.read_now("missing").await.unwrap_err();
    assert!(matches!(e, PlcError::Rejected(_)) && !e.is_link(), "{e:?}");

    // 5. 杀掉服务端 → 链路错误、点位离线 → 拉起 → 恢复，读到这次掉线的拨号次数。
    server.kill();
    wait_until("link loss", Duration::from_secs(5), || {
        rig.engine.status().state != LinkState::Connected
    })
    .await;
    assert!(rig
        .engine
        .status()
        .last_error
        .as_ref()
        .is_some_and(PlcError::is_link));
    assert!(matches!(
        rig.engine.values()["i16"].error,
        Some(PlcError::Offline(_))
    ));
    tokio::time::sleep(Duration::from_millis(600)).await;
    server.spawn();
    let polls = rig.engine.status().poll_count;
    wait_until("recovery", Duration::from_secs(10), || {
        let s = rig.engine.status();
        s.state == LinkState::Connected && s.poll_count > polls
    })
    .await;
    let status = rig.engine.status();
    println!(
        "recovered after {} dials, last dial error: {:?}",
        status.outage_dials, status.last_dial_error
    );
    assert!(status.outage_dials >= 2, "{status:?}");
    assert!(status
        .last_dial_error
        .as_ref()
        .is_some_and(PlcError::is_link));
    rig.engine.disconnect().await;
}
