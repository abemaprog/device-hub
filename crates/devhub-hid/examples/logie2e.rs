//! Logitech ドライバの E2E (アプリと同じ `connect → scan → driver → read/apply` 経路)。
//!
//! `cargo run -p devhub-hid --example logie2e`
//!
//! 読み出し → ポーリング 500Hz → 元に戻す → DPI ステージ追加 → 元に戻す、
//! の各段で read() の結果が書いた値と一致するか確認する。最後は必ず元の状態に戻す。

use devhub_core::backend::*;
use devhub_core::model::*;

fn main() {
    let hub = devhub_hid::connect().expect("connect");
    let devs = hub.scan().expect("scan");
    let info = devs
        .iter()
        .find(|d| d.driver == DriverKind::Logitech)
        .expect("Logitech デバイスなし");
    println!("scan: {} {:?} {}", info.id, info.transport, info.name);
    let mut drv = None;
    for _ in 0..10 {
        match hub.driver(&info.id) {
            Ok(d) => {
                drv = Some(d);
                break;
            }
            Err(e) => {
                println!("open: {e} (マウスを動かしてください)");
                std::thread::sleep(std::time::Duration::from_secs(3));
            }
        }
    }
    let d = drv.expect("open");
    let i = d.info();
    println!(
        "info: name={} fw={:?} dongle={:?} battery={:?}",
        i.name, i.firmware, i.dongle_firmware, i.battery
    );
    let caps = d.caps();
    let m = caps.mouse.as_ref().unwrap();
    println!(
        "caps: dpi {}..{} stages<= {} xy={} buttons={:?}",
        m.dpi_min,
        m.dpi_max,
        m.max_stages,
        m.xy_split,
        m.buttons.iter().map(|b| b.name.as_str()).collect::<Vec<_>>()
    );
    for s in &caps.settings {
        println!("setting: {} {:?}", s.id, s.kind);
    }
    let orig = d.read().expect("read");
    let ms = orig.mouse.clone().unwrap();
    println!("state: profile={} settings={:?}", orig.profile.active, orig.settings);
    println!("  stages={:?} active={}", ms.stages.iter().map(|s| (s.x, s.y)).collect::<Vec<_>>(), ms.active_stage);
    println!("  buttons={:?}", ms.buttons);
    println!("battery: {:?}", d.battery());

    let check = |label: &str, ok: bool| println!("[{}] {label}", if ok { "OK" } else { "NG" });

    // 1) ポーリング 500Hz → 戻す
    let rate0 = orig.settings.get("report_rate").cloned().unwrap();
    d.apply(&Change::Setting { id: "report_rate".into(), value: SettingValue::Choice(500) }).expect("rate");
    let s = d.read().unwrap();
    check("report_rate=500", s.settings.get("report_rate") == Some(&SettingValue::Choice(500)));
    d.apply(&Change::Setting { id: "report_rate".into(), value: rate0.clone() }).expect("rate back");
    check("report_rate restored", d.read().unwrap().settings.get("report_rate") == Some(&rate0));

    // 2) DPI ステージ追加 (800 を先頭に) → 戻す
    let mut st = ms.stages.clone();
    if st.len() < 5 {
        st.insert(0, DpiStage { x: 800, y: None, color: Rgb { r: 0, g: 0, b: 0 }, enabled: true });
        let act = ms.active_stage + 1;
        d.apply(&Change::DpiStages { stages: st.clone(), active: act }).expect("dpi");
        let s = d.read().unwrap().mouse.unwrap();
        check(
            "dpi stages written",
            s.stages.iter().map(|x| x.x).collect::<Vec<_>>() == st.iter().map(|x| x.x).collect::<Vec<_>>()
                && s.active_stage == act,
        );
        d.apply(&Change::DpiStages { stages: ms.stages.clone(), active: ms.active_stage }).expect("dpi back");
    }
    let fin = d.read().unwrap();
    check("final state == original", fin.mouse == orig.mouse && fin.settings == orig.settings);
}
