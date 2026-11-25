//! Logitech HID++ 実機プローブ (読み取りのみ。書き込みは一切しない)。
//!
//! `cargo run -p devhub-hid --example logiprobe`
//!
//! 046D の HID インターフェースを列挙し、HID++ long コレクションを開いて
//! スロット 1..6 / 0xFF に ping、応答した機器の feature 一覧・名前・FW・電池・
//! DPI・レポートレート・オンボードプロファイル (ディレクトリと全セクタ) を出力する。

use devhub_hid::hidpp::{feat, Hidpp};
use std::time::Duration;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}

fn main() {
    let api = hidapi::HidApi::new().expect("hidapi");
    let mut long_paths = vec![];
    for d in api.device_list().filter(|d| d.vendor_id() == 0x046D) {
        println!(
            "{:04x}:{:04x} if={} up={:04x} u={:02x} prod={:?} sn={:?}",
            d.vendor_id(),
            d.product_id(),
            d.interface_number(),
            d.usage_page(),
            d.usage(),
            d.product_string(),
            d.serial_number()
        );
        if d.usage_page() == 0xFF00 && d.usage() == 0x02 {
            long_paths.push((d.product_id(), d.path().to_owned()));
        }
    }
    for (pid, path) in long_paths {
        println!("\n===== {pid:04x} long collection");
        for idx in [1u8, 2, 3, 4, 5, 6, 0xFF] {
            let dev = api.open_path(&path).expect("open");
            let pp = Hidpp::new(dev, idx);
            match pp.ping(Duration::from_millis(400)) {
                Ok((maj, min)) => {
                    println!("-- index {idx:#04x}: HID++ {maj}.{min}");
                    probe(&pp);
                }
                Err(e) => println!("-- index {idx:#04x}: {e:?}"),
            }
        }
    }
}

fn probe(pp: &Hidpp) {
    match pp.list_features() {
        Ok(fs) => {
            for (i, (id, ty, ver)) in fs.iter().enumerate() {
                println!(
                    "   [{i:2}] {id:04x} v{ver} type={ty:02x} {}",
                    feat::name(*id)
                );
            }
        }
        Err(e) => println!("   features: {e}"),
    }
    println!("   name: {:?}", pp.device_name());
    println!("   fw: {:?}", pp.firmware());
    for (id, f, p) in [
        (feat::BATTERY_STATUS, 0u8, vec![]),
        (feat::BATTERY_VOLTAGE, 0, vec![]),
        (feat::UNIFIED_BATTERY, 0, vec![]),
        (feat::UNIFIED_BATTERY, 1, vec![]),
        (feat::ADJUSTABLE_DPI, 0, vec![]),
        (feat::ADJUSTABLE_DPI, 1, vec![0]),
        (feat::ADJUSTABLE_DPI, 2, vec![0]),
        (feat::EXTENDED_DPI, 0, vec![]),
        (feat::REPORT_RATE, 0, vec![]),
        (feat::REPORT_RATE, 1, vec![]),
        (feat::EXT_REPORT_RATE, 0, vec![]),
        (feat::EXT_REPORT_RATE, 1, vec![]),
        (feat::EXT_REPORT_RATE, 2, vec![]),
        (feat::EXTENDED_DPI, 1, vec![0]),
        (feat::EXTENDED_DPI, 2, vec![0, 0, 0]),
        (feat::EXTENDED_DPI, 2, vec![0, 0, 1]),
        (feat::EXTENDED_DPI, 3, vec![0]),
        (feat::EXTENDED_DPI, 5, vec![0]),
        (feat::EXTENDED_DPI, 7, vec![0]),
        (feat::ONBOARD_PROFILES, 0, vec![]),
        (feat::ONBOARD_PROFILES, 2, vec![]),
        (feat::ONBOARD_PROFILES, 4, vec![]),
        (feat::ONBOARD_PROFILES, 0xB, vec![]),
        (feat::RGB_EFFECTS, 0, vec![0xFF, 0, 0]),
        (feat::COLOR_LED_EFFECTS, 0, vec![]),
        (feat::REPROG_CONTROLS_V4, 0, vec![]),
        (feat::WIRELESS_STATUS, 0, vec![]),
    ] {
        match pp.fcall(id, f, &p) {
            Ok(r) => println!("   {} fn{f} {} -> {}", feat::name(id), hex(&p), hex(&r)),
            Err(e) => println!("   {} fn{f} -> {e}", feat::name(id)),
        }
    }
    // ReprogControlsV4: 各コントロールの CID/TID/フラグ。
    if let Ok(r) = pp.fcall(feat::REPROG_CONTROLS_V4, 0, &[]) {
        for i in 0..r[0] {
            if let Ok(c) = pp.fcall(feat::REPROG_CONTROLS_V4, 1, &[i]) {
                println!("   ctrl[{i}] {}", hex(&c[..9]));
            }
        }
    }
    // オンボードプロファイル。
    let Ok(d) = pp.fcall(feat::ONBOARD_PROFILES, 0, &[]) else {
        return;
    };
    let sector_size = u16::from_be_bytes([d[7], d[8]]) as usize;
    let profiles = d[3];
    println!(
        "   onboard: model={} pfmt={} mfmt={} profiles={} oob={} buttons={} sectors={} sector_size={}",
        d[0], d[1], d[2], d[3], d[4], d[5], d[6], sector_size
    );
    let read = |sector: u16| -> Option<Vec<u8>> {
        let mut out = vec![];
        let mut off = 0usize;
        while off < sector_size {
            let o = off.min(sector_size - 16);
            let r = pp
                .fcall(
                    feat::ONBOARD_PROFILES,
                    5,
                    &[(sector >> 8) as u8, sector as u8, (o >> 8) as u8, o as u8],
                )
                .ok()?;
            out.truncate(o);
            out.extend_from_slice(&r);
            off = o + 16;
        }
        Some(out)
    };
    let dump = |label: &str, sector: u16| {
        match read(sector) {
            Some(s) => {
                let crc = devhub_hid::hidpp::crc_ccitt(&s[..s.len() - 2]);
                let stored = u16::from_be_bytes([s[s.len() - 2], s[s.len() - 1]]);
                println!(
                    "   sector {sector:#06x} ({label}) crc calc={crc:04x} stored={stored:04x}"
                );
                for (i, ch) in s.chunks(16).enumerate() {
                    println!("     {:04x}: {}", i * 16, hex(ch));
                }
            }
            None => println!("   sector {sector:#06x} ({label}): read failed"),
        }
    };
    dump("directory", 0x0000);
    for p in 1..=profiles as u16 {
        dump("user", p);
    }
    for p in 1..=profiles as u16 {
        dump("rom", 0x0100 + p);
    }
}
