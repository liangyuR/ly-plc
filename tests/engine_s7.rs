//! 引擎对着进程内的 S7 测试服务端（`support`）：按需读、写后确认、只写点位、原始字节点位、
//! 带类型的错误、重拨统计。

mod support;

use std::time::{Duration, Instant};

use ly_plc::{Access, DataType, EdgeMode, LinkState, LogQuery, PlcError, PlcValue, VerifyOptions};
use serde_json::json;
use support::{config, point, rig, wait_until, Mode, Read, S7Server, AREA_DB};

const SLOW_POLL_MS: u64 = 2_000;

fn verify(attempts: u32) -> VerifyOptions {
    VerifyOptions {
        attempts,
        delay: Duration::from_millis(5),
    }
}

// ---- 1. 按需读 / 写后确认 ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_now_bypasses_the_poll_cache() {
    let server = S7Server::start().await;
    let rig = rig(config(
        server.port,
        SLOW_POLL_MS,
        vec![
            point("a", "DB1.DBW4", DataType::U16),
            point("b", "DB1.DBB10", DataType::U8),
        ],
    ));
    rig.start().await;
    assert_eq!(rig.engine.values()["a"].value, Some(PlcValue::Int(0)));

    server.set_db(1, 4, &[0x12, 0x34]);
    server.clear_reads();
    let started = Instant::now();
    assert_eq!(rig.engine.read_now("a").await, Ok(PlcValue::Int(0x1234)));
    assert!(started.elapsed() < Duration::from_millis(200));
    // 只读了这个点位自己的区域，缓存还是旧值（下一轮轮询要等 2 s）。
    assert_eq!(
        server.reads(),
        [Read {
            area: AREA_DB,
            db: 1,
            start: 4,
            count: 2
        }]
    );
    assert_eq!(rig.engine.values()["a"].value, Some(PlcValue::Int(0)));
    rig.engine.disconnect().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_now_is_serialized_with_polling() {
    // 轮询与按需读在同一条 S7 连接上；不串行的话请求 / 应答会交错，读出别的点位的值。
    let server = S7Server::start().await;
    let points = (0..8)
        .map(|i| {
            point(
                &format!("p{i}"),
                &format!("DB1.DBB{}", i * 20),
                DataType::U8,
            )
        })
        .collect();
    for i in 0..8 {
        server.set_db(1, i * 20, &[i as u8 + 1]);
    }
    let rig = std::sync::Arc::new(rig(config(server.port, 20, points)));
    rig.start().await;
    let tasks: Vec<_> = (0..64)
        .map(|n| {
            let rig = rig.clone();
            tokio::spawn(async move {
                let i = n % 8;
                rig.engine.read_now(&format!("p{i}")).await.map(|v| (i, v))
            })
        })
        .collect();
    for t in tasks {
        let (i, v) = t.await.unwrap().unwrap();
        assert_eq!(v, PlcValue::Int(i as i64 + 1));
    }
    assert_eq!(rig.engine.status().state, LinkState::Connected);
    rig.engine.disconnect().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poll_now_publishes_a_read_taken_after_the_write_without_waiting_for_the_period() {
    let server = S7Server::start().await;
    let rig = rig(config(
        server.port,
        SLOW_POLL_MS,
        vec![
            point("out", "DB1.DBW4", DataType::U16),
            point("in", "DB1.DBW8", DataType::U16),
        ],
    ));
    rig.start().await;
    let polls = rig.engine.status().poll_count;

    rig.engine.write_point("out", &json!(0x1234)).await.unwrap();
    server.set_db(1, 8, &[0, 7]);
    let started = Instant::now();
    rig.engine.poll_now().await.unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(rig.engine.status().poll_count, polls + 1);
    let values = rig.engine.values();
    assert_eq!(values["out"].value, Some(PlcValue::Int(0x1234)));
    assert_eq!(values["in"].value, Some(PlcValue::Int(7)));

    rig.engine.disconnect().await;
    assert!(matches!(
        rig.engine.poll_now().await,
        Err(PlcError::Offline(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verified_write_confirms_without_waiting_for_polls() {
    let server = S7Server::start().await;
    let rig = rig(config(
        server.port,
        SLOW_POLL_MS,
        vec![
            point("result", "DB1.DBB0", DataType::U8),
            point("f", "DB1.DBD4", DataType::F32),
            point("flag", "DB1.DBX8.3", DataType::Bool),
        ],
    ));
    rig.start().await;

    let started = Instant::now();
    let report = rig
        .engine
        .write_point_verified("result", &json!(2), verify(12))
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "{:?}",
        started.elapsed()
    );
    assert_eq!((report.attempts, report.written), (1, true));
    assert_eq!(report.readback, Some(PlcValue::Int(2)));
    assert_eq!(server.db(1, 0, 1), [2]);

    // 期望值按读取的解码算，f32 的舍入不会造成假的不一致。
    let report = rig
        .engine
        .write_point_verified("f", &json!(0.1), verify(1))
        .await
        .unwrap();
    assert_eq!(report.attempts, 1);
    let report = rig
        .engine
        .write_point_verified("flag", &json!(true), verify(1))
        .await
        .unwrap();
    assert_eq!(report.readback, Some(PlcValue::Bool(true)));
    assert_eq!(server.db(1, 8, 1), [0b1000]);
    rig.engine.disconnect().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verified_write_rewrites_until_the_value_sticks() {
    let server = S7Server::start().await;
    let rig = rig(config(
        server.port,
        SLOW_POLL_MS,
        vec![point("result", "DB1.DBB0", DataType::U8)],
    ));
    rig.start().await;

    // PLC 程序把前两次写入改回 9。
    server.pin_db(1, 0, 9, Some(2));
    let report = rig
        .engine
        .write_point_verified("result", &json!(1), verify(5))
        .await
        .unwrap();
    assert_eq!(report.attempts, 3);
    assert_eq!(report.readback, Some(PlcValue::Int(1)));
    assert_eq!(server.writes(), 3);

    // 一直被改回去：用尽次数，Mismatch，报告里带最后一次读回。
    server.pin_db(1, 0, 7, None);
    let err = rig
        .engine
        .write_point_verified("result", &json!(1), verify(4))
        .await
        .unwrap_err();
    assert!(matches!(err.error, PlcError::Mismatch(_)), "{err:?}");
    assert_eq!(err.report.attempts, 4);
    assert!(err.report.written);
    assert_eq!(err.report.readback, Some(PlcValue::Int(7)));
    rig.engine.disconnect().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verified_write_stops_at_the_first_hard_error() {
    let server = S7Server::start().await;
    let rig = rig(config(
        server.port,
        SLOW_POLL_MS,
        vec![
            point("missing", "DB9.DBB0", DataType::U8),
            point("result", "DB1.DBB0", DataType::U8),
        ],
    ));
    rig.start().await;
    let err = rig
        .engine
        .write_point_verified("missing", &json!(1), verify(5))
        .await
        .unwrap_err();
    assert!(matches!(err.error, PlcError::Rejected(_)), "{err:?}");
    assert_eq!((err.report.attempts, err.report.written), (1, false));
    let err = rig
        .engine
        .write_point_verified("result", &json!(300), verify(5))
        .await
        .unwrap_err();
    assert!(matches!(err.error, PlcError::Invalid(_)), "{err:?}");
    rig.engine.disconnect().await;
}

// ---- 2. 只写、不轮询的点位 ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unpolled_points_are_written_but_never_polled() {
    let server = S7Server::start().await;
    let mut mirror = point("mirror", "DB1.DBB1.40", DataType::Bytes);
    mirror.poll = false;
    let mut ack = point("ack", "DB2.DBB0", DataType::U8);
    ack.poll = false;
    // 不存在的 DB：如果被轮询，每轮都会读失败并写进日志。
    let mut ghost = point("ghost", "DB9.DBB0", DataType::U8);
    ghost.poll = false;
    let rig = rig(config(
        server.port,
        20,
        vec![point("hb", "DB1.DBB0", DataType::U8), mirror, ack, ghost],
    ));
    rig.start().await;
    rig.wait_polls(5).await;

    // 轮询只读心跳那 1 个字节：紧挨着的镜像区没有并进同一块，DB2 / DB9 根本没读。
    let reads = server.reads();
    assert!(!reads.is_empty());
    assert!(
        reads.iter().all(|r| *r
            == Read {
                area: AREA_DB,
                db: 1,
                start: 0,
                count: 1
            }),
        "{reads:?}"
    );
    let values = rig.engine.values();
    assert_eq!(values.keys().collect::<Vec<_>>(), ["hb"]);

    // 写照常；按需读也可以。
    rig.engine.write_point("ack", &json!(42)).await.unwrap();
    assert_eq!(server.db(2, 0, 1), [42]);
    assert_eq!(rig.engine.read_now("ack").await, Ok(PlcValue::Int(42)));
    assert!(matches!(
        rig.engine.read_now("ghost").await,
        Err(PlcError::Rejected(_))
    ));
    rig.wait_polls(3).await;
    assert_eq!(rig.engine.values().len(), 1);

    // 日志里没有它们的读取错误（按需读不记日志）。
    tokio::time::sleep(Duration::from_millis(200)).await;
    let errors = rig
        .logbook
        .query(&LogQuery {
            categories: vec!["error".into()],
            ..LogQuery::default()
        })
        .unwrap();
    assert_eq!(errors.total, 0, "{:?}", errors.items);
    rig.engine.disconnect().await;
}

#[test]
fn unpolled_points_cannot_have_edges() {
    let mut p = point("x", "DB1.DBX0.0", DataType::Bool);
    p.poll = false;
    p.edge = EdgeMode::Rising;
    assert!(config(102, 50, vec![p.clone()]).validate().is_err());
    p.edge = EdgeMode::None;
    assert!(config(102, 50, vec![p]).validate().is_ok());
    // 默认轮询；旧配置里没有 poll 字段时照旧。
    let p: ly_plc::PlcPoint =
        serde_json::from_value(json!({"id": "a", "name": "a", "address": "DB1.DBW0"})).unwrap();
    assert!(p.poll);
}

// ---- 3. 原始字节点位 ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bytes_point_keeps_a_string_header_the_string_type_rejects() {
    let server = S7Server::start().await;
    // PLC 声明的容量 (254) 大于配置长度 (40)：STRING 点位整个报错，Bytes 点位原样给出 42 字节。
    let mut raw = vec![254u8, 5];
    raw.extend_from_slice(b"VIN01");
    server.set_db(1, 100, &raw);
    let rig = rig(config(
        server.port,
        20,
        vec![
            point("vin_raw", "DB1.DBB100.42", DataType::Bytes),
            point("vin_str", "DB1.DBB100.40", DataType::String),
        ],
    ));
    rig.start().await;
    let values = rig.engine.values();
    let PlcValue::Bytes(bytes) = values["vin_raw"].value.clone().unwrap() else {
        panic!("{:?}", values["vin_raw"]);
    };
    assert_eq!(bytes.len(), 42);
    assert_eq!(&bytes[..7], &raw[..]);
    assert!(matches!(values["vin_str"].error, Some(PlcError::Decode(_))));
    // 两个点位区域相同，合并成一次读。
    server.clear_reads();
    rig.wait_polls(2).await;
    assert!(server
        .reads()
        .iter()
        .all(|r| (r.start, r.count) == (100, 42)));

    let mut header = vec![40u8, 3, b'A', b'B', b'C'];
    header.resize(42, 0);
    rig.engine
        .write_point("vin_raw", &json!(header))
        .await
        .unwrap();
    assert_eq!(server.db(1, 100, 42), header);
    assert_eq!(
        rig.engine.read_now("vin_str").await,
        Ok(PlcValue::Text("ABC".into()))
    );
    assert!(matches!(
        rig.engine.write_point("vin_raw", &json!([1, 2, 3])).await,
        Err(PlcError::Invalid(_))
    ));
    rig.engine.disconnect().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn smart_string_point_uses_a_single_length_byte_on_the_wire() {
    let server = S7Server::start().await;
    server.set_db(1, 100, &[3, b'A', b'B', b'C']);
    let rig = rig(config(
        server.port,
        20,
        vec![point("sn", "VB100.10", DataType::SmartString)],
    ));
    rig.start().await;
    assert_eq!(
        rig.engine.values()["sn"].value,
        Some(PlcValue::Text("ABC".into()))
    );
    rig.engine.write_point("sn", &json!("SN-42")).await.unwrap();
    assert_eq!(
        server.db(1, 100, 11),
        [5, b'S', b'N', b'-', b'4', b'2', 0, 0, 0, 0, 0]
    );
    rig.engine.disconnect().await;
}

#[test]
fn point_beyond_one_frame_of_the_configured_pdu_fails_validation() {
    // PDU 240：单帧读最多 222 字节、写最多 212 字节；SmartString `.n` 占 n + 1 字节。
    for (address, access, ok) in [
        ("VB0.221", Access::Read, true),
        ("VB0.222", Access::Read, false),
        ("VB0.211", Access::ReadWrite, true),
        ("VB0.212", Access::ReadWrite, false),
    ] {
        let mut p = point("sn", address, DataType::SmartString);
        p.access = access;
        let mut cfg = config(102, 50, vec![p]);
        cfg.connection.s7.pdu_size = 240;
        assert_eq!(cfg.validate().is_ok(), ok, "{address} {access:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn point_beyond_one_frame_of_the_negotiated_pdu_is_rejected_without_a_request() {
    // 请求 PDU 480 时校验放行，CPU（如 200 SMART）协商为 240：单帧最多读 222 字节、写 212 字节。
    let server = S7Server::start().await;
    server.set_pdu(240);
    let mut big = point("big", "VB0.254", DataType::SmartString);
    big.access = Access::Read;
    let rig = rig(config(
        server.port,
        20,
        vec![big, point("sn", "DB2.DBB0.220", DataType::SmartString)],
    ));
    rig.start().await;
    rig.wait_polls(2).await;

    let values = rig.engine.values();
    assert!(
        matches!(values["big"].error, Some(PlcError::Rejected(_))),
        "{:?}",
        values["big"]
    );
    assert_eq!(values["sn"].value, Some(PlcValue::Text(String::new())));
    assert!(
        server.reads().iter().all(|r| r.count <= 222),
        "{:?}",
        server.reads()
    );
    assert!(matches!(
        rig.engine.write_point("sn", &json!("SN-42")).await,
        Err(PlcError::Rejected(_))
    ));
    assert_eq!(server.writes(), 0);
    let status = rig.engine.status();
    assert_eq!((status.state, status.dials), (LinkState::Connected, 1));
    rig.engine.disconnect().await;
}

// ---- 4. 带类型的错误 ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_and_invalid_requests_keep_the_link() {
    let server = S7Server::start().await;
    let mut ro = point("ro", "DB1.DBB0", DataType::U8);
    ro.access = Access::Read;
    let rig = rig(config(
        server.port,
        20,
        vec![ro, point("missing", "DB9.DBW0", DataType::U16)],
    ));
    assert!(matches!(
        rig.engine.read_now("ro").await,
        Err(PlcError::Offline(_))
    ));
    rig.start().await;

    let missing = &rig.engine.values()["missing"];
    assert!(
        matches!(missing.error, Some(PlcError::Rejected(_))),
        "{missing:?}"
    );
    let shown = serde_json::to_value(missing).unwrap();
    assert_eq!(shown["errorKind"], "rejected");
    assert!(shown["error"].as_str().unwrap().contains("S7 访问失败"));

    assert!(matches!(
        rig.engine.write_point("missing", &json!(1)).await,
        Err(PlcError::Rejected(_))
    ));
    assert!(matches!(
        rig.engine.read_now("missing").await,
        Err(PlcError::Rejected(_))
    ));
    for (id, value) in [
        ("ro", json!(1)),
        ("nope", json!(1)),
        ("missing", json!("x")),
    ] {
        let e = rig.engine.write_point(id, &value).await.unwrap_err();
        assert!(matches!(e, PlcError::Invalid(_)), "{id}: {e:?}");
        assert!(!e.is_link());
    }
    // `?` 进 String 的老调用方式仍然可用。
    async fn legacy(rig: &support::Rig) -> Result<(), String> {
        rig.engine.write_point("ro", &json!(1)).await?;
        Ok(())
    }
    assert!(legacy(&rig).await.unwrap_err().contains("不可写"));

    rig.wait_polls(2).await;
    let status = rig.engine.status();
    assert_eq!(status.state, LinkState::Connected);
    assert_eq!((status.dials, server.accepted()), (1, 1));
    rig.engine.disconnect().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_on_demand_tears_the_link_down_and_reconnects() {
    let server = S7Server::start().await;
    let rig = rig(config(
        server.port,
        SLOW_POLL_MS,
        vec![point("a", "DB1.DBB0", DataType::U8)],
    ));
    rig.start().await;

    server.set_mode(Mode::Stall);
    let e = rig.engine.read_now("a").await.unwrap_err();
    assert!(matches!(e, PlcError::Timeout(_)), "{e:?}");
    assert!(e.is_link());
    // 连接已关掉：迟到的应答不会错位到下一个请求上。
    assert!(matches!(
        rig.engine.read_now("a").await,
        Err(PlcError::Offline(_))
    ));
    server.set_mode(Mode::Normal);
    // 轮询任务睡醒后按掉线处理（缓存标记离线），然后重连。
    wait_until("reconnect", Duration::from_secs(5), || {
        rig.engine.status().dials >= 2 && rig.engine.status().state == LinkState::Connected
    })
    .await;
    assert!(matches!(
        rig.engine.status().last_error,
        Some(PlcError::Timeout(_))
    ));
    assert_eq!(rig.engine.read_now("a").await, Ok(PlcValue::Int(0)));
    assert_eq!(server.accepted(), 2);
    rig.engine.disconnect().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_connection_marks_points_offline() {
    let server = S7Server::start().await;
    let rig = rig(config(
        server.port,
        20,
        vec![point("a", "DB1.DBB0", DataType::U8)],
    ));
    rig.start().await;
    server.set_mode(Mode::RejectCotp);
    server.kill_connections();
    wait_until("offline", Duration::from_secs(3), || {
        rig.engine.values()["a"].error.is_some()
    })
    .await;
    let a = &rig.engine.values()["a"];
    assert!(matches!(a.error, Some(PlcError::Offline(_))), "{a:?}");
    assert_eq!(serde_json::to_value(a).unwrap()["errorKind"], "offline");
    let status = rig.engine.status();
    assert!(
        matches!(
            status.last_error,
            Some(PlcError::Link(_) | PlcError::Rejected(_))
        ),
        "{status:?}"
    );
    rig.engine.disconnect().await;
}

// ---- 5. 重拨统计 ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dial_statistics_track_each_outage() {
    let server = S7Server::start().await;
    server.set_mode(Mode::RejectCotp);
    let rig = rig(config(
        server.port,
        20,
        vec![point("a", "DB1.DBB0", DataType::U8)],
    ));
    rig.engine.connect().await;
    // `dials` 在拨号开始时就加 1；等的是两次拨号都已失败（`error_count`）。
    wait_until("two failed dials", Duration::from_secs(5), || {
        rig.engine.status().error_count >= 2
    })
    .await;
    let status = rig.engine.status();
    assert_ne!(status.state, LinkState::Connected);
    assert!(
        matches!(&status.last_dial_error, Some(PlcError::Rejected(m)) if m.contains("COTP")),
        "{status:?}"
    );
    server.set_mode(Mode::Normal);
    wait_until("first poll", Duration::from_secs(5), || {
        rig.engine.status().poll_count >= 1
    })
    .await;
    let first = rig.engine.status();
    assert_eq!(first.state, LinkState::Connected);
    assert!(first.outage_dials >= 3);
    assert_eq!(u64::from(first.outage_dials), first.dials);
    assert!(first.last_dial_error.is_some(), "kept after success");

    // 第二次掉线：计数从 0 重新开始，恢复后读到的就是这次用了几次拨号。
    server.set_mode(Mode::RejectCotp);
    server.kill_connections();
    wait_until("link loss", Duration::from_secs(3), || {
        rig.engine.status().state != LinkState::Connected
    })
    .await;
    // 一次链路失败 + 两次拨号失败。
    wait_until("two failed redials", Duration::from_secs(5), || {
        rig.engine.status().error_count >= first.error_count + 3
    })
    .await;
    server.set_mode(Mode::Normal);
    let polls = rig.engine.status().poll_count;
    wait_until("recovery", Duration::from_secs(5), || {
        rig.engine.status().poll_count > polls
    })
    .await;
    let second = rig.engine.status();
    assert_eq!(second.state, LinkState::Connected);
    assert_eq!(u64::from(second.outage_dials), second.dials - first.dials);
    assert!(second.outage_dials >= 3);
    assert!(matches!(
        second.last_dial_error,
        Some(PlcError::Rejected(_))
    ));

    let json = serde_json::to_value(&second).unwrap();
    assert_eq!(json["lastDialError"]["kind"], "rejected");
    assert_eq!(json["outageDials"], second.outage_dials);

    // 重新 connect() 清零。
    rig.engine.disconnect().await;
    rig.engine.connect().await;
    let fresh = rig.engine.status();
    assert!(fresh.dials <= 1 && fresh.last_dial_error.is_none() && fresh.last_error.is_none());
    rig.engine.disconnect().await;
}

// ---- 日志关联上下文 ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn log_context_tags_changes_and_writes_with_the_context_point_read_in_the_same_poll() {
    let server = S7Server::start().await;
    let mut flag = point("flag", "DB1.DBX4.0", DataType::Bool);
    flag.access = Access::Read;
    flag.edge = EdgeMode::Rising;
    let rig = rig(config(
        server.port,
        20,
        vec![
            point("seq", "DB1.DBW0", DataType::U16),
            flag,
            point("out", "DB1.DBB6", DataType::U8),
        ],
    ));
    rig.start().await;
    rig.engine.set_log_context(Some(ly_plc::LogContext {
        point_id: "seq".into(),
        label: "requestSeq".into(),
    }));

    // 序号与信号在同一轮里变：信号的变化日志归到新序号。
    server.set_db(1, 0, &[0, 12, 0, 0, 1]);
    rig.wait_polls(2).await;
    rig.engine.write_point("out", &json!(5)).await.unwrap();
    let tagged = |context: &str| {
        rig.logbook
            .query(&LogQuery {
                context: Some(context.into()),
                ..LogQuery::default()
            })
            .unwrap()
            .items
    };
    wait_until("tagged logs", Duration::from_secs(3), || {
        tagged("requestSeq=12").len() >= 4
    })
    .await;
    let entries = tagged("requestSeq=12");
    for (point, category) in [
        ("seq", "value"),
        ("flag", "value"),
        ("flag", "edge"),
        ("out", "write"),
    ] {
        assert!(
            entries
                .iter()
                .any(|e| e.point_id.as_deref() == Some(point) && e.category == category),
            "{point} {category}: {entries:?}"
        );
    }
    assert!(tagged("requestSeq=0")
        .iter()
        .all(|e| e.point_id.as_deref() != Some("flag")));

    rig.engine.disconnect().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let last = rig
        .logbook
        .query(&LogQuery {
            limit: Some(1),
            ..LogQuery::default()
        })
        .unwrap()
        .items;
    assert_eq!(last[0].category, "connection");
    assert_eq!(last[0].context, None, "断开后不再附带上下文");
}

#[test]
fn logbook_created_by_0_3_gains_the_context_column_on_open() {
    let db = std::env::temp_dir().join(format!("ly-plc-old-schema-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&db);
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE plc_log (id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL, level TEXT NOT NULL,
             category TEXT NOT NULL, point_id TEXT, point_name TEXT, message TEXT NOT NULL, old_value TEXT, new_value TEXT);
             INSERT INTO plc_log (ts, level, category, message) VALUES (1, 'info', 'connection', '旧记录');",
        )
        .unwrap();
    }
    let sink: ly_plc::EventSink = std::sync::Arc::new(|_| {});
    let logbook = ly_plc::Logbook::open(&db, sink, 0).unwrap();
    let page = logbook.query(&LogQuery::default()).unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(
        (
            page.items[0].message.as_str(),
            page.items[0].context.as_deref()
        ),
        ("旧记录", None)
    );
    drop(logbook);
    let _ = std::fs::remove_file(&db);
}
