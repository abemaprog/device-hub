//! オンボードプロファイル書き込みの検証 (format 7 のフィールド確定用)。
//!
//! `cargo run -p devhub-hid --example logiwrite -- <backup.bin>`
//!
//! 現在プロファイルのセクタを backup に保存 → byte0 (レポートレート) を 500Hz に
//! 書き換え → 本体の実測値 (0x8061 fn2) を確認 → DPI index 0 の値を 1000 に変えて
//! 0x2202 の現在値を確認 → 最後に元のセクタへ戻して読み戻し一致を確認する。

use devhub_hid::hidpp::{crc_ccitt, feat, Hidpp};
use std::time::Duration;

const SIZE: usize = 255;

fn read(pp: &Hidpp, sector: u16) -> Vec<u8> {
    let mut out = vec![];
    let mut o = 0;
    while o < SIZE {
        let at = o.min(SIZE - 16);
        let r = pp
            .fcall(feat::ONBOARD_PROFILES, 5, &[(sector >> 8) as u8, sector as u8, 0, at as u8])
            .expect("read");
        out.truncate(at);
        out.extend_from_slice(&r);
        o = at + 16;
    }
    out
}

fn write(pp: &Hidpp, sector: u16, data: &mut [u8]) {
    let crc = crc_ccitt(&data[..SIZE - 2]).to_be_bytes();
    data[SIZE - 2] = crc[0];
    data[SIZE - 1] = crc[1];
    pp.fcall(
        feat::ONBOARD_PROFILES,
        6,
        &[(sector >> 8) as u8, sector as u8, 0, 0, 0, SIZE as u8],
    )
    .expect("addr");
    for ch in data.chunks(16) {
        let mut b = [0u8; 16];
        b[..ch.len()].copy_from_slice(ch);
        pp.fcall(feat::ONBOARD_PROFILES, 7, &b).expect("write");
    }
    pp.fcall(feat::ONBOARD_PROFILES, 8, &[]).expect("end");
}

fn state(pp: &Hidpp, label: &str) {
    let rate = pp.fcall(feat::EXT_REPORT_RATE, 2, &[]).map(|r| r[0]);
    let dpi = pp
        .fcall(feat::EXTENDED_DPI, 5, &[0])
        .map(|r| u16::from_be_bytes([r[1], r[2]]));
    let idx = pp.fcall(feat::ONBOARD_PROFILES, 0xB, &[]).map(|r| r[0]);
    println!("[{label}] rate_code={rate:?} dpi={dpi:?} dpi_index={idx:?}");
}

fn main() {
    let backup = std::env::args().nth(1).expect("backup path");
    let api = hidapi::HidApi::new().unwrap();
    let info = api
        .device_list()
        .find(|d| d.vendor_id() == 0x046D && d.usage_page() == 0xFF00 && d.usage() == 2)
        .expect("receiver");
    let pp = Hidpp::new(info.open_device(&api).unwrap(), 1);
    pp.ping(Duration::from_millis(500)).expect("mouse asleep");
    let cur = pp.fcall(feat::ONBOARD_PROFILES, 4, &[]).unwrap()[1];
    let sector = cur as u16;
    let orig = read(&pp, sector);
    std::fs::write(&backup, &orig).unwrap();
    println!("profile {cur} backed up to {backup}");
    state(&pp, "before");

    let reselect = |pp: &Hidpp| {
        pp.fcall(feat::ONBOARD_PROFILES, 3, &[0, cur]).unwrap();
        std::thread::sleep(Duration::from_millis(200));
    };

    // 1) byte0 = 2 (500Hz)
    let mut t = orig.clone();
    t[0] = 2;
    write(&pp, sector, &mut t);
    reselect(&pp);
    state(&pp, "byte0=2");

    // 2) byte1 = 2 だけ (byte0 は元)
    let mut t = orig.clone();
    t[1] = 2;
    write(&pp, sector, &mut t);
    reselect(&pp);
    state(&pp, "byte1=2");

    // 3) 既定 DPI ステージの X/Y を 1000 に
    let mut t = orig.clone();
    let di = t[2] as usize;
    let o = 4 + di * 5;
    t[o..o + 4].copy_from_slice(&[0xE8, 0x03, 0xE8, 0x03]);
    write(&pp, sector, &mut t);
    reselect(&pp);
    state(&pp, "dpi[default]=1000");

    // 4) setCurrentDpiIndex (fn 0xC) — 有効な別ステージがあれば
    let valid: Vec<u8> = (0..5u8)
        .filter(|i| {
            let o = 4 + *i as usize * 5;
            u16::from_le_bytes([orig[o], orig[o + 1]]) != 0
        })
        .collect();
    println!("valid stages {valid:?}");

    // 復元
    let mut t = orig.clone();
    write(&pp, sector, &mut t);
    reselect(&pp);
    let back = read(&pp, sector);
    println!("restored, identical = {}", back == orig);
    state(&pp, "restored");
}
