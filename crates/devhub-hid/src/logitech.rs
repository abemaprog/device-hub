//! Logitech G (LIGHTSPEED / 有線) マウス — HID++ 2.0 オンボードプロファイル駆動。
//!
//! G HUB を使わずに設定を**マウス本体のオンボードメモリ**へ書き込む。
//! G HUB 常駐中はマウスがホストモード (0x8100 mode=2) にされることがあるので、
//! 開いた時点でオンボードモード (mode=1) に戻し、プロファイルセクタを直接書き換える。
//! 本アプリが終了していても設定はマウス側に残る。
//!
//! プロファイルのバイト配置は形式 (0x8100 fn0 の profile format) で 2 系統ある。
//! 詳細と実機ダンプ・書き込み検証の記録は `docs/LOGITECH_HIDPP.md`。
//!
//! - **Legacy (format 1..3)** — G PRO Wireless / G502 / G903 世代。libratbag と同じ配置。
//!   レート = ms (1=1000Hz)、DPI = u16 LE ×5 (0x03〜)、ボタン 0x20〜、LED 0xD0〜。
//! - **Modern (format 4+)** — PRO X SUPERLIGHT 2 ("PRO X 2", format 7) で実機確定。
//!   0x00 有線レート / 0x01 無線レート (0x8061 コード 0=125Hz … 6=8kHz)、
//!   0x02 既定 DPI index、0x03 シフト DPI index、0x04〜 DPI ×5 (X u16LE, Y u16LE, LOD)、
//!   ボタン 0x30〜 (×16)、G-Shift 0x70〜、名前 0xA0〜 (UTF-16LE)。
//!
//! ボタン割当 4B (両形式共通):
//! - `80 01 hh ll` マウスボタン (bitmask BE: 1=左 2=右 4=中 8=戻る 0x10=進む)
//! - `80 02 mm kk` キー (修飾 / HID usage)
//! - `80 03 hh ll` コンシューマ (usage BE)
//! - `90 ss 00 00` 特殊 (DPI/プロファイル/チルト)
//! - `FF ..`       無効
//! - `00 ..`       マクロ (本アプリでは往復保存のみ)

use crate::hidpp::{crc_ccitt, feat, Hidpp};
use devhub_core::backend::*;
use devhub_core::model::*;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const OB_MODE_ONBOARD: u8 = 1;
const DPI_SLOTS: usize = 5;

/// プロファイルセクタのバイト配置。
#[derive(Clone, Copy, Debug, PartialEq)]
struct Layout {
    /// レポートレートのバイト位置 (Modern は [有線, 無線])。
    rate: &'static [usize],
    /// レートの表現: true = 0x8061 コード、false = ms。
    rate_code: bool,
    def_dpi: usize,
    shift_dpi: usize,
    dpi: usize,
    /// DPI 1 エントリのバイト数 (2 = X のみ、5 = X,Y,LOD)。
    dpi_stride: usize,
    buttons: usize,
    name: usize,
    /// LED (11B) の位置。LED 機能が無い機種は None。
    leds: Option<usize>,
    angle_snap: Option<usize>,
}

const LEGACY: Layout = Layout {
    rate: &[0x00],
    rate_code: false,
    def_dpi: 0x01,
    shift_dpi: 0x02,
    dpi: 0x03,
    dpi_stride: 2,
    buttons: 0x20,
    name: 0xA0,
    leds: Some(0xD0),
    angle_snap: Some(0x11),
};

const MODERN: Layout = Layout {
    rate: &[0x00, 0x01],
    rate_code: true,
    def_dpi: 0x02,
    shift_dpi: 0x03,
    dpi: 0x04,
    dpi_stride: 5,
    buttons: 0x30,
    name: 0xA0,
    leds: None,
    angle_snap: None,
};

/// オンボード機能の記述子 (0x8100 fn0)。
#[derive(Clone, Copy, Debug)]
struct Onboard {
    profiles: u8,
    buttons: u8,
    sector_size: usize,
    layout: Layout,
}

/// 読み込んだプロファイル 1 枚。
#[derive(Clone, Debug)]
struct Profile {
    /// 1 始まり。
    index: u8,
    /// 書き込み先のユーザーセクタ番号。
    sector: u16,
    /// ユーザーセクタが無い/壊れていて ROM 既定を読んだ。
    from_rom: bool,
    data: Vec<u8>,
}

/// レポートレート (Hz) ↔ 0x8061 コード。
const RATE_CODES: [u32; 7] = [125, 250, 500, 1000, 2000, 4000, 8000];

pub struct LogiDriver {
    pp: Arc<Hidpp>,
    info: Mutex<DeviceInfo>,
    caps: Capabilities,
    ob: Option<Onboard>,
    /// 選べるレポートレート (Hz)。
    rates: Vec<u32>,
    /// 有効な DPI 値の区間 `(from, to, step)` (0x2202 / 0x2201 のリスト)。
    dpi_ranges: Vec<(u32, u32, u32)>,
    dpi_feat: Option<u16>,
    battery_feat: Option<u16>,
    cache: Mutex<Option<Profile>>,
    stop: Arc<AtomicBool>,
}

impl Drop for LogiDriver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

pub fn open(
    dev: hidapi::HidDevice,
    path: &std::ffi::CStr,
    mut info: DeviceInfo,
    events: Option<Arc<dyn Fn(HubEvent) + Send + Sync>>,
) -> HubResult<LogiDriver> {
    let candidates: &[u8] = if info.transport == Transport::Wired {
        &[0xFF]
    } else {
        &[1, 2, 3, 4, 5, 6]
    };
    // スロット探索: 同じパスを別ハンドルで開いて ping する。
    let api = hidapi::HidApi::new().map_err(|e| HubError::Io(e.to_string()))?;
    let mut found = None;
    // 無線マウスは数秒の無操作でスリープし ping に答えない — 2 周試す。
    'outer: for _ in 0..2 {
        for idx in candidates {
            let Ok(h) = api.open_path(path) else { continue };
            if Hidpp::new(h, *idx).ping(Duration::from_millis(250)).is_ok() {
                found = Some(*idx);
                break 'outer;
            }
        }
    }
    let Some(index) = found else {
        // ペア済みだが応答しない = スリープ中 (LIGHTSPEED は数秒の無操作で眠る)。
        return Err(HubError::Io(
            "マウスがスリープ中です — マウスを動かしてから再試行してください".into(),
        ));
    };
    let pp = Arc::new(Hidpp::new(dev, index));

    if let Ok(name) = pp.device_name() {
        if !name.is_empty() {
            info.name = format!("Logicool G {name}");
        }
    }
    if let Ok(fw) = pp.firmware() {
        info.firmware = fw.iter().find(|(t, _)| *t == 0).map(|(_, s)| s.clone());
    }
    info.asleep = false;
    info.supported = true;

    let has = |id| pp.feature(id).ok().flatten().is_some();
    let battery_feat = [
        feat::UNIFIED_BATTERY,
        feat::BATTERY_VOLTAGE,
        feat::BATTERY_STATUS,
    ]
    .into_iter()
    .find(|f| has(*f));
    let dpi_feat = [feat::EXTENDED_DPI, feat::ADJUSTABLE_DPI]
        .into_iter()
        .find(|f| has(*f));
    let has_led = has(feat::COLOR_LED_EFFECTS) || has(feat::RGB_EFFECTS);

    let ob = pp
        .fcall(feat::ONBOARD_PROFILES, 0, &[])
        .ok()
        .map(|d| {
            let mut layout = if d[1] >= 4 { MODERN } else { LEGACY };
            if !has_led {
                layout.leds = None;
            }
            Onboard {
                profiles: d[3].max(1),
                buttons: d[5].min(16),
                sector_size: u16::from_be_bytes([d[7], d[8]]) as usize,
                layout,
            }
        })
        .filter(|o| o.sector_size >= 0xFF);

    // ホストモードのままだと本体が保存プロファイルを使わない — オンボードへ戻す。
    if ob.is_some() {
        if let Ok(m) = pp.fcall(feat::ONBOARD_PROFILES, 2, &[]) {
            if m[0] != OB_MODE_ONBOARD {
                pp.fcall(feat::ONBOARD_PROFILES, 1, &[OB_MODE_ONBOARD]).ok();
            }
        }
    }

    let rates = report_rates(&pp, ob.as_ref());
    let dpi_ranges = dpi_feat
        .and_then(|f| dpi_ranges(&pp, f))
        .unwrap_or_else(|| vec![(100, 25600, 50)]);
    let caps = build_caps(ob.as_ref(), &rates, &dpi_ranges, battery_feat.is_some());

    let stop = Arc::new(AtomicBool::new(false));
    let drv = LogiDriver {
        pp,
        info: Mutex::new(info.clone()),
        caps,
        ob,
        rates,
        dpi_ranges,
        dpi_feat,
        battery_feat,
        cache: Mutex::new(None),
        stop: stop.clone(),
    };
    if let Ok(b) = drv.battery() {
        drv.info.lock().unwrap_or_else(|p| p.into_inner()).battery = Some(b);
    }
    if let Some(ev) = events {
        spawn_notify(path.to_owned(), index, info.id.clone(), ev, stop);
    }
    Ok(drv)
}

fn report_rates(pp: &Hidpp, ob: Option<&Onboard>) -> Vec<u32> {
    let mut out = vec![];
    let modern = ob.map(|o| o.layout.rate_code).unwrap_or(false);
    if modern {
        if let Ok(r) = pp.fcall(feat::EXT_REPORT_RATE, 1, &[]) {
            let mask = u16::from_be_bytes([r[0], r[1]]);
            for (i, hz) in RATE_CODES.iter().enumerate() {
                if mask & (1 << i) != 0 {
                    out.push(*hz);
                }
            }
        }
    } else if let Ok(r) = pp.fcall(feat::REPORT_RATE, 0, &[]) {
        for bit in (0..8u32).rev() {
            if r[0] & (1 << bit) != 0 {
                out.push(1000 / (bit + 1));
            }
        }
        out.sort();
    }
    if out.is_empty() {
        out = vec![125, 250, 500, 1000];
    }
    out
}

/// DPI リスト → 区間 `(from, to, step)` の列。
///
/// リストは u16 BE の列で、`0xE000|step` は直前値から次の値まで step 刻みの範囲を表す。
/// 0x2202 はページ (13B) をまたいで続き、0 で終わる (PRO X 2 実機:
/// 100 /1/ 200 /2/ 500 /5/ 1000 /10/ 2000 /20/ 5000 /50/ 10000 …)。
fn dpi_ranges(pp: &Hidpp, f: u16) -> Option<Vec<(u32, u32, u32)>> {
    let mut bytes = vec![];
    if f == feat::EXTENDED_DPI {
        for page in 0..8u8 {
            let r = pp.fcall(f, 2, &[0, 0, page]).ok()?;
            bytes.extend_from_slice(&r[3..16]);
            if r[3..16].chunks(2).any(|c| c.len() == 2 && c == [0, 0]) {
                break;
            }
        }
    } else {
        let r = pp.fcall(f, 1, &[0]).ok()?;
        bytes.extend_from_slice(&r[1..]);
    }
    Some(parse_dpi_list(&bytes)).filter(|v| !v.is_empty())
}

fn parse_dpi_list(bytes: &[u8]) -> Vec<(u32, u32, u32)> {
    let vals: Vec<u32> = bytes
        .chunks(2)
        .filter(|c| c.len() == 2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]) as u32)
        .take_while(|v| *v != 0)
        .collect();
    let mut out = vec![];
    let mut i = 0;
    while i < vals.len() {
        let v = vals[i];
        if v >= 0xE000 {
            i += 1;
            continue;
        }
        match (vals.get(i + 1), vals.get(i + 2)) {
            (Some(s), Some(to)) if *s >= 0xE000 && *to < 0xE000 => {
                out.push((v, *to, (s & 0x1FFF).max(1)));
                i += 2;
            }
            _ => {
                // 区間の終端が次の始点を兼ねる — 単独の値は直前区間の終端でなければ追加。
                if out.last().is_none_or(|l: &(u32, u32, u32)| l.1 != v) {
                    out.push((v, v, 1));
                }
                i += 1;
            }
        }
    }
    out
}

/// 値を有効な DPI の最も近い値に丸める。
fn snap_dpi(ranges: &[(u32, u32, u32)], dpi: u32) -> u32 {
    let mut best = dpi;
    let mut dist = u32::MAX;
    for &(from, to, step) in ranges {
        let c = if dpi <= from {
            from
        } else if dpi >= to {
            to
        } else {
            let k = ((dpi - from) as f32 / step as f32).round() as u32;
            (from + k * step).min(to)
        };
        let d = c.abs_diff(dpi);
        if d < dist {
            dist = d;
            best = c;
        }
    }
    best
}

fn rate_label(hz: u32) -> String {
    if hz >= 1000 {
        format!("{}K", hz / 1000)
    } else {
        hz.to_string()
    }
}

/// ボタン番号 (プロファイル内の並び) → 表示名。
/// 5 ボタン機 (SUPERLIGHT 系) は左・右・中・戻る・進む。G PRO Wireless は
/// 左右両側のサイドボタン + 底面 DPI ボタンで 8 個。
fn button_name(i: usize, count: u8) -> String {
    let wide = count >= 8;
    match i {
        0 => "左クリック".into(),
        1 => "右クリック".into(),
        2 => "ホイールクリック".into(),
        3 if wide => "戻る (左側)".into(),
        4 if wide => "進む (左側)".into(),
        3 => "戻る".into(),
        4 => "進む".into(),
        5 if wide => "DPI ボタン (底面)".into(),
        6 if wide => "戻る (右側)".into(),
        7 if wide => "進む (右側)".into(),
        _ => format!("ボタン {}", i + 1),
    }
}

fn build_caps(
    ob: Option<&Onboard>,
    rates: &[u32],
    dpi: &[(u32, u32, u32)],
    battery: bool,
) -> Capabilities {
    use section::*;
    let mut c = Capabilities::empty();
    c.battery = battery;
    let dpi_min = dpi.iter().map(|r| r.0).min().unwrap_or(100);
    let dpi_max = dpi.iter().map(|r| r.1).max().unwrap_or(25600);
    let Some(ob) = ob else {
        // オンボードプロファイル非対応機: 現在 DPI 単発だけ。
        c.mouse = Some(MouseCaps {
            dpi_min,
            dpi_max,
            dpi_step: 50,
            min_stages: 1,
            max_stages: 1,
            xy_split: false,
            stage_colors: false,
            stage_toggle: false,
            buttons: vec![],
            button_kinds: vec![],
            require_left_click: true,
            macros: None,
            live_buttons: vec![],
            adaptive_dpi: None,
        });
        return c;
    };
    let l = ob.layout;
    c.settings = vec![SettingSpec::choice(
        "report_rate",
        PERFORMANCE,
        "ポーリングレート",
        rates
            .iter()
            .map(|hz| ChoiceOpt::new(*hz as i64, rate_label(*hz)))
            .collect(),
    )
    .help("マウスのデータ更新頻度 (Hz)。オンボードプロファイルに保存されます")];
    if l.rate_code && rates.iter().any(|r| *r > 1000) {
        c.settings[0] = c.settings[0]
            .clone()
            .confirm("2000Hz 以上は CPU 負荷と電池消費が大きくなります。切り替えますか？");
    }
    if l.angle_snap.is_some() {
        c.settings.push(
            SettingSpec::toggle("angle_snap", SENSOR, "直線補正")
                .help("マウス入力をより直線的にします (Angle Snapping)"),
        );
    }
    c.actions = vec![
        ActionSpec::new("reset_profile", SYSTEM, "プロファイルを初期値に戻す")
            .help("現在のオンボードプロファイルを ROM の工場出荷値で上書きします")
            .danger("現在のプロファイルの DPI・ボタン設定が初期化されます"),
    ];
    c.profiles = (ob.profiles > 1).then_some(ProfileCaps {
        count: ob.profiles,
        renamable: false,
    });
    c.mouse = Some(MouseCaps {
        dpi_min,
        dpi_max,
        dpi_step: 50,
        min_stages: 1,
        max_stages: DPI_SLOTS,
        xy_split: l.dpi_stride >= 4,
        stage_colors: false,
        stage_toggle: false,
        buttons: (0..ob.buttons as usize)
            .map(|i| ButtonSlot {
                name: button_name(i, ob.buttons),
            })
            .collect(),
        button_kinds: vec![
            ButtonActionKind::Mouse,
            ButtonActionKind::Key,
            ButtonActionKind::Media,
            ButtonActionKind::Dpi,
            ButtonActionKind::ProfileCycle,
            ButtonActionKind::Disabled,
        ],
        require_left_click: true,
        macros: None,
        live_buttons: vec![],
        adaptive_dpi: None,
    });
    if l.leds.is_some() {
        c.lighting = vec![LightArea {
            id: 0,
            name: "ロゴ".into(),
            effects: vec![
                LightEffect::new(LED_FIXED, "固定").color().still(),
                LightEffect::new(LED_CYCLE, "カラーサイクル"),
                LightEffect::new(LED_BREATH, "ブリージング").color(),
            ],
            brightness_max: 100,
            speed_max: 100,
            directions: vec![],
            switchable: true,
            zones: vec![],
            per_key: false,
            direct_drive: false,
            live_readback: false,
        }];
    }
    c
}

// ---------------------------------------------------------------- LED (Legacy 形式のみ)

const LED_OFF: u8 = 0x00;
const LED_FIXED: u8 = 0x01;
const LED_CYCLE: u8 = 0x03;
const LED_BREATH: u8 = 0x0A;

/// speed 0..100 ↔ 周期 ms (速いほど短い)。
fn speed_to_period(speed: u8) -> u16 {
    let s = speed.min(100) as u32;
    (20000 - s * (20000 - 1000) / 100) as u16
}
fn period_to_speed(p: u16) -> u8 {
    let p = (p as u32).clamp(1000, 20000);
    ((20000 - p) * 100 / (20000 - 1000)) as u8
}

fn decode_led(b: &[u8]) -> LightingState {
    let mut st = LightingState {
        on: b[0] != LED_OFF,
        effect: b[0],
        color: Rgb { r: 0, g: 0, b: 0 },
        brightness: 100,
        speed: 50,
        direction: 0,
        per_key: BTreeMap::new(),
        zone_on: vec![],
        palette: vec![],
        palette_sel: 0,
        color_correction: None,
    };
    match b[0] {
        LED_FIXED => st.color = Rgb { r: b[1], g: b[2], b: b[3] },
        LED_CYCLE => {
            st.speed = period_to_speed(u16::from_be_bytes([b[6], b[7]]));
            st.brightness = if b[8] == 0 { 100 } else { b[8].min(100) };
        }
        LED_BREATH => {
            st.color = Rgb { r: b[1], g: b[2], b: b[3] };
            st.speed = period_to_speed(u16::from_be_bytes([b[4], b[5]]));
            st.brightness = if b[7] == 0 { 100 } else { b[7].min(100) };
        }
        _ => st.effect = LED_FIXED,
    }
    st
}

fn encode_led(st: &LightingState) -> [u8; 11] {
    let mut b = [0u8; 11];
    if !st.on {
        return b;
    }
    let bright = st.brightness.clamp(1, 100);
    match st.effect {
        LED_CYCLE => {
            b[0] = LED_CYCLE;
            b[6..8].copy_from_slice(&speed_to_period(st.speed).to_be_bytes());
            b[8] = bright;
        }
        LED_BREATH => {
            b[0] = LED_BREATH;
            b[1..4].copy_from_slice(&[st.color.r, st.color.g, st.color.b]);
            b[4..6].copy_from_slice(&speed_to_period(st.speed).to_be_bytes());
            b[7] = bright;
        }
        _ => {
            b[0] = LED_FIXED;
            // 固定色に輝度フィールドは無いので色に乗算する。
            let k = bright as u16;
            b[1] = (st.color.r as u16 * k / 100) as u8;
            b[2] = (st.color.g as u16 * k / 100) as u8;
            b[3] = (st.color.b as u16 * k / 100) as u8;
        }
    }
    b
}

// ---------------------------------------------------------------- ボタン

mod special {
    pub const WHEEL_LEFT: u8 = 0x01;
    pub const WHEEL_RIGHT: u8 = 0x02;
    pub const DPI_UP: u8 = 0x03;
    pub const DPI_DOWN: u8 = 0x04;
    pub const DPI_CYCLE: u8 = 0x05;
    pub const DPI_SHIFT: u8 = 0x07;
    pub const PROFILE_CYCLE: u8 = 0x0A;
}

const MEDIA: &[(MediaFn, u16)] = &[
    (MediaFn::PlayPause, 0x00CD),
    (MediaFn::Stop, 0x00B7),
    (MediaFn::Next, 0x00B5),
    (MediaFn::Prev, 0x00B6),
    (MediaFn::VolumeUp, 0x00E9),
    (MediaFn::VolumeDown, 0x00EA),
    (MediaFn::Mute, 0x00E2),
    (MediaFn::Calculator, 0x0192),
    (MediaFn::MyComputer, 0x0194),
    (MediaFn::Browser, 0x0196),
    (MediaFn::Mail, 0x018A),
    (MediaFn::BrightnessUp, 0x006F),
    (MediaFn::BrightnessDown, 0x0070),
    (MediaFn::Refresh, 0x0227),
];

const MOUSE_BITS: &[(MouseFn, u16)] = &[
    (MouseFn::Left, 0x0001),
    (MouseFn::Right, 0x0002),
    (MouseFn::Middle, 0x0004),
    (MouseFn::Back, 0x0008),
    (MouseFn::Forward, 0x0010),
];

fn decode_button(b: &[u8], stages: &[DpiStage], shift_idx: usize) -> ButtonAction {
    match (b[0], b[1]) {
        (0x80, 0x01) => {
            let bits = u16::from_be_bytes([b[2], b[3]]);
            MOUSE_BITS
                .iter()
                .find(|(_, m)| *m == bits)
                .map(|(f, _)| ButtonAction::Mouse(*f))
                .unwrap_or_else(|| raw(b))
        }
        (0x80, 0x02) => ButtonAction::Key(KeyStroke {
            mods: b[2],
            usage: b[3] as u16,
        }),
        (0x80, 0x03) => {
            let u = u16::from_be_bytes([b[2], b[3]]);
            MEDIA
                .iter()
                .find(|(_, c)| *c == u)
                .map(|(f, _)| ButtonAction::Media(*f))
                .unwrap_or_else(|| raw(b))
        }
        (0x90, s) => match s {
            special::WHEEL_LEFT => ButtonAction::Mouse(MouseFn::ScrollLeft),
            special::WHEEL_RIGHT => ButtonAction::Mouse(MouseFn::ScrollRight),
            special::DPI_UP => ButtonAction::Dpi(DpiFn::Up),
            special::DPI_DOWN => ButtonAction::Dpi(DpiFn::Down),
            special::DPI_CYCLE => ButtonAction::Dpi(DpiFn::Cycle),
            special::DPI_SHIFT => ButtonAction::Dpi(DpiFn::Shift(
                stages.get(shift_idx).map(|s| s.x).unwrap_or(400),
            )),
            special::PROFILE_CYCLE => ButtonAction::ProfileCycle,
            _ => raw(b),
        },
        (0xFF, _) => ButtonAction::Disabled,
        _ => raw(b),
    }
}

fn raw(b: &[u8]) -> ButtonAction {
    ButtonAction::Unsupported {
        ty: b[0],
        params: b[1..4].to_vec(),
    }
}

/// 割当 → 4B。DPI シフトはシフト先 DPI も返す。
fn encode_button(a: &ButtonAction) -> HubResult<([u8; 4], Option<u32>)> {
    let sp = |s: u8| [0x90, s, 0, 0];
    Ok(match a {
        ButtonAction::Disabled => ([0xFF, 0xFF, 0xFF, 0xFF], None),
        ButtonAction::Mouse(MouseFn::ScrollLeft) => (sp(special::WHEEL_LEFT), None),
        ButtonAction::Mouse(MouseFn::ScrollRight) => (sp(special::WHEEL_RIGHT), None),
        ButtonAction::Mouse(f) => {
            let bits = MOUSE_BITS
                .iter()
                .find(|(m, _)| m == f)
                .map(|(_, b)| *b)
                .ok_or(HubError::Unsupported("このマウス機能"))?
                .to_be_bytes();
            ([0x80, 0x01, bits[0], bits[1]], None)
        }
        ButtonAction::Key(k) => ([0x80, 0x02, k.mods, k.usage.min(0xFF) as u8], None),
        ButtonAction::Media(m) => {
            let u = MEDIA
                .iter()
                .find(|(f, _)| f == m)
                .map(|(_, c)| *c)
                .ok_or(HubError::Unsupported("このメディアキー"))?
                .to_be_bytes();
            ([0x80, 0x03, u[0], u[1]], None)
        }
        ButtonAction::Dpi(DpiFn::Cycle) => (sp(special::DPI_CYCLE), None),
        ButtonAction::Dpi(DpiFn::Up) => (sp(special::DPI_UP), None),
        ButtonAction::Dpi(DpiFn::Down) => (sp(special::DPI_DOWN), None),
        ButtonAction::Dpi(DpiFn::Shift(d)) => (sp(special::DPI_SHIFT), Some(*d)),
        ButtonAction::ProfileCycle => (sp(special::PROFILE_CYCLE), None),
        ButtonAction::Unsupported { ty, params } => {
            let mut b = [*ty, 0, 0, 0];
            for (i, p) in params.iter().take(3).enumerate() {
                b[1 + i] = *p;
            }
            (b, None)
        }
        _ => return Err(HubError::Unsupported("この割り当て")),
    })
}

// ---------------------------------------------------------------- 本体

impl LogiDriver {
    fn ob(&self) -> HubResult<Onboard> {
        self.ob.ok_or(HubError::Unsupported("オンボードプロファイル"))
    }

    fn read_sector(&self, sector: u16) -> HubResult<Vec<u8>> {
        let size = self.ob()?.sector_size;
        let mut out = Vec::with_capacity(size);
        let mut o = 0usize;
        while o < size {
            let at = o.min(size - 16);
            let r = self.pp.fcall(
                feat::ONBOARD_PROFILES,
                5,
                &[(sector >> 8) as u8, sector as u8, (at >> 8) as u8, at as u8],
            )?;
            out.truncate(at);
            out.extend_from_slice(&r);
            o = at + 16;
        }
        Ok(out)
    }

    /// セクタ全体を書き込む (CRC は末尾 2B に計算して入れる)。
    fn write_sector(&self, sector: u16, data: &mut [u8]) -> HubResult<()> {
        let size = self.ob()?.sector_size;
        if data.len() != size {
            return Err(HubError::Invalid("sector size".into()));
        }
        let crc = crc_ccitt(&data[..size - 2]).to_be_bytes();
        data[size - 2] = crc[0];
        data[size - 1] = crc[1];
        self.pp.fcall(
            feat::ONBOARD_PROFILES,
            6,
            &[
                (sector >> 8) as u8,
                sector as u8,
                0,
                0,
                (size >> 8) as u8,
                size as u8,
            ],
        )?;
        for ch in data.chunks(16) {
            let mut b = [0u8; 16];
            b[..ch.len()].copy_from_slice(ch);
            self.pp.fcall(feat::ONBOARD_PROFILES, 7, &b)?;
        }
        self.pp.fcall(feat::ONBOARD_PROFILES, 8, &[])?;
        Ok(())
    }

    /// ディレクトリ (sector 0): 4B エントリ `[sector BE, enabled, ?]`、0xFFFF 終端。
    fn directory(&self) -> HubResult<Vec<(u16, bool)>> {
        let d = self.read_sector(0)?;
        let mut v = vec![];
        for e in d.chunks(4).take(16) {
            if e.len() < 4 {
                break;
            }
            let s = u16::from_be_bytes([e[0], e[1]]);
            if s == 0xFFFF || s == 0 {
                break;
            }
            v.push((s, e[2] != 0));
        }
        Ok(v)
    }

    fn current_index(&self) -> HubResult<u8> {
        let r = self.pp.fcall(feat::ONBOARD_PROFILES, 4, &[])?;
        Ok(r[1].max(1))
    }

    fn crc_ok(&self, data: &[u8]) -> bool {
        let n = data.len();
        n > 2 && crc_ccitt(&data[..n - 2]) == u16::from_be_bytes([data[n - 2], data[n - 1]])
    }

    /// 現在プロファイルを読む (キャッシュ更新)。
    fn load_profile(&self) -> HubResult<Profile> {
        let idx = self.current_index()?;
        let dir = self.directory().unwrap_or_default();
        let user = dir.get(idx as usize - 1).map(|(s, _)| *s);
        let rom = || self.read_sector(0x0100 + idx as u16);
        let p = match user {
            Some(sector) => {
                let data = self.read_sector(sector)?;
                if self.crc_ok(&data) {
                    Profile {
                        index: idx,
                        sector,
                        from_rom: false,
                        data,
                    }
                } else {
                    // CRC 不一致 = 本体も ROM 既定にフォールバックしている。
                    Profile {
                        index: idx,
                        sector,
                        from_rom: true,
                        data: rom().or_else(|_| self.read_sector(0x0101))?,
                    }
                }
            }
            None => Profile {
                index: idx,
                sector: idx as u16,
                from_rom: true,
                data: rom().or_else(|_| self.read_sector(0x0101))?,
            },
        };
        *self.cache.lock().unwrap_or_else(|p| p.into_inner()) = Some(p.clone());
        Ok(p)
    }

    fn profile(&self) -> HubResult<Profile> {
        if let Some(p) = self.cache.lock().unwrap_or_else(|p| p.into_inner()).clone() {
            return Ok(p);
        }
        self.load_profile()
    }

    /// 編集したプロファイルを書き込み、本体に読み直させる。
    fn commit(&self, mut p: Profile) -> HubResult<()> {
        let ob = self.ob()?;
        self.write_sector(p.sector, &mut p.data)?;
        if p.from_rom {
            // ディレクトリに無ければ全プロファイル分のエントリを作る
            // (未作成プロファイルは ROM をコピーして入れる)。
            let mut dir = self.directory().unwrap_or_default();
            if dir.len() < p.index as usize {
                for i in dir.len() as u16 + 1..=ob.profiles as u16 {
                    if i != p.sector {
                        let mut rom = self
                            .read_sector(0x0100 + i)
                            .or_else(|_| self.read_sector(0x0101))?;
                        self.write_sector(i, &mut rom)?;
                    }
                    dir.push((i, i == 1 || i == p.sector));
                }
                let mut d = vec![0xFFu8; ob.sector_size];
                for (k, (s, en)) in dir.iter().enumerate() {
                    d[k * 4] = (s >> 8) as u8;
                    d[k * 4 + 1] = *s as u8;
                    d[k * 4 + 2] = *en as u8;
                    d[k * 4 + 3] = 0xFF;
                }
                self.write_sector(0, &mut d)?;
            }
            p.from_rom = false;
        }
        // 再選択で本体がセクタを読み直す (現在 DPI index は既定に戻るので退避して戻す)。
        let dpi_idx = self
            .pp
            .fcall(feat::ONBOARD_PROFILES, 0xB, &[])
            .ok()
            .map(|r| r[0]);
        self.pp.fcall(feat::ONBOARD_PROFILES, 3, &[0, p.index]).ok();
        if let Some(i) = dpi_idx {
            if (i as usize) < self.stages(&p.data).len() {
                self.pp.fcall(feat::ONBOARD_PROFILES, 0xC, &[i]).ok();
            }
        }
        *self.cache.lock().unwrap_or_else(|p| p.into_inner()) = Some(p);
        Ok(())
    }

    /// 有効な DPI ステージ (0 / 0xFFFF の空きスロットは詰める)。
    fn stages(&self, data: &[u8]) -> Vec<DpiStage> {
        let Some(ob) = self.ob else { return vec![] };
        let l = ob.layout;
        (0..DPI_SLOTS)
            .filter_map(|i| {
                let o = l.dpi + i * l.dpi_stride;
                let x = u16::from_le_bytes([data[o], data[o + 1]]);
                if x == 0 || x == 0xFFFF {
                    return None;
                }
                let y = (l.dpi_stride >= 4)
                    .then(|| u16::from_le_bytes([data[o + 2], data[o + 3]]) as u32)
                    .filter(|y| *y != 0 && *y != x as u32);
                Some(DpiStage {
                    x: x as u32,
                    y,
                    color: Rgb { r: 0, g: 160, b: 255 },
                    enabled: true,
                })
            })
            .collect()
    }

    /// 有効ステージ index → スロット番号 (空きスロットを飛ばした並び)。
    fn slot_of(&self, data: &[u8], stage: usize) -> usize {
        let Some(ob) = self.ob else { return stage };
        let l = ob.layout;
        (0..DPI_SLOTS)
            .filter(|i| {
                let o = l.dpi + i * l.dpi_stride;
                let x = u16::from_le_bytes([data[o], data[o + 1]]);
                x != 0 && x != 0xFFFF
            })
            .nth(stage)
            .unwrap_or(0)
    }

    /// スロット番号 → 有効ステージ index。
    fn stage_of(&self, data: &[u8], slot: usize) -> usize {
        let Some(ob) = self.ob else { return slot };
        let l = ob.layout;
        (0..slot.min(DPI_SLOTS))
            .filter(|i| {
                let o = l.dpi + i * l.dpi_stride;
                let x = u16::from_le_bytes([data[o], data[o + 1]]);
                x != 0 && x != 0xFFFF
            })
            .count()
    }

    fn write_stages(&self, data: &mut [u8], stages: &[DpiStage]) {
        let l = self.ob.expect("onboard").layout;
        for i in 0..DPI_SLOTS {
            let o = l.dpi + i * l.dpi_stride;
            let (x, y) = match stages.get(i) {
                Some(s) => {
                    let x = snap_dpi(&self.dpi_ranges, s.x);
                    (x as u16, snap_dpi(&self.dpi_ranges, s.y.unwrap_or(x)) as u16)
                }
                None => (0, 0),
            };
            data[o..o + 2].copy_from_slice(&x.to_le_bytes());
            if l.dpi_stride >= 4 {
                data[o + 2..o + 4].copy_from_slice(&y.to_le_bytes());
                // LOD バイトは既存値を保持。空きスロットも ROM と同じ 02 のまま。
            }
        }
    }

    fn rate_hz(&self, data: &[u8]) -> u32 {
        let l = self.ob.expect("onboard").layout;
        // Modern は無線側 (0x01) が実効値 (実機で確認)。
        let b = data[*l.rate.last().unwrap()];
        if l.rate_code {
            RATE_CODES.get(b as usize).copied().unwrap_or(1000)
        } else {
            1000 / b.max(1) as u32
        }
    }
}

impl DeviceDriver for LogiDriver {
    fn info(&self) -> DeviceInfo {
        self.info.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn caps(&self) -> Capabilities {
        self.caps.clone()
    }

    fn read(&self) -> HubResult<DeviceState> {
        let mut st = DeviceState {
            profile: ProfileState {
                active: 0,
                names: vec![],
            },
            settings: BTreeMap::new(),
            mouse: None,
            keyboard: None,
            lighting: BTreeMap::new(),
            peq: None,
        };
        let Some(ob) = self.ob else {
            let dpi = self.current_dpi()?;
            st.mouse = Some(MouseState {
                stages: vec![DpiStage {
                    x: dpi,
                    y: None,
                    color: Rgb { r: 0, g: 160, b: 255 },
                    enabled: true,
                }],
                active_stage: 0,
                buttons: vec![],
                macros: vec![],
                adaptive: None,
            });
            return Ok(st);
        };
        let l = ob.layout;
        let p = self.load_profile()?;
        let d = &p.data;
        st.profile.active = p.index - 1;
        st.profile.names = (0..ob.profiles)
            .map(|i| format!("プロファイル {}", i + 1))
            .collect();
        let name = utf16_name(&d[l.name..l.name + 48]);
        if !name.is_empty() && name != "PROFILE_NAME_DEFAULT" {
            if let Some(n) = st.profile.names.get_mut(p.index as usize - 1) {
                *n = name;
            }
        }
        st.settings.insert(
            "report_rate".into(),
            SettingValue::Choice(self.rate_hz(d) as i64),
        );
        if let Some(o) = l.angle_snap {
            st.settings
                .insert("angle_snap".into(), SettingValue::Bool(d[o] == 1));
        }

        let stages = self.stages(d);
        let active = self
            .pp
            .fcall(feat::ONBOARD_PROFILES, 0xB, &[])
            .map(|r| r[0] as usize)
            .unwrap_or(d[l.def_dpi] as usize);
        let active = self
            .stage_of(d, active)
            .min(stages.len().saturating_sub(1));
        let shift = self.stage_of(d, d[l.shift_dpi] as usize);
        let buttons = (0..ob.buttons as usize)
            .map(|i| {
                let o = l.buttons + i * 4;
                decode_button(&d[o..o + 4], &stages, shift)
            })
            .collect();
        st.mouse = Some(MouseState {
            stages,
            active_stage: active,
            buttons,
            macros: vec![],
            adaptive: None,
        });
        if let Some(o) = l.leds {
            st.lighting.insert(0, decode_led(&d[o..o + 11]));
        }
        Ok(st)
    }

    fn apply(&self, change: &Change) -> HubResult<()> {
        let Some(ob) = self.ob else {
            return match change {
                Change::DpiStages { stages, active } => {
                    let s = stages
                        .get(*active)
                        .or(stages.first())
                        .ok_or(HubError::Invalid("DPI".into()))?;
                    self.set_current_dpi(s.x)
                }
                _ => Err(HubError::Unsupported("この変更")),
            };
        };
        let l = ob.layout;
        match change {
            Change::ActiveStage(i) => {
                let p = self.profile()?;
                let slot = self.slot_of(&p.data, *i);
                self.pp.fcall(feat::ONBOARD_PROFILES, 0xC, &[slot as u8])?;
                Ok(())
            }
            Change::Profile(i) => {
                // 未作成のユーザープロファイルは ROM のコピーで作ってから切り替える。
                let dir = self.directory().unwrap_or_default();
                if (*i as usize) >= dir.len() {
                    let mut p = self.profile()?;
                    p.index = i + 1;
                    p.sector = (i + 1) as u16;
                    p.from_rom = true;
                    p.data = self
                        .read_sector(0x0100 + p.sector)
                        .or_else(|_| self.read_sector(0x0101))?;
                    self.commit(p)?;
                }
                self.pp.fcall(feat::ONBOARD_PROFILES, 3, &[0, i + 1])?;
                *self.cache.lock().unwrap_or_else(|p| p.into_inner()) = None;
                Ok(())
            }
            Change::DpiStages { stages, active } => {
                if stages.is_empty() || stages.len() > DPI_SLOTS {
                    return Err(HubError::Invalid("DPI ステージ数".into()));
                }
                let mut p = self.profile()?;
                let old = self.stages(&p.data);
                let shift_dpi = old
                    .get(self.stage_of(&p.data, p.data[l.shift_dpi] as usize))
                    .map(|s| s.x);
                self.write_stages(&mut p.data, stages);
                let a = (*active).min(stages.len() - 1);
                p.data[l.def_dpi] = a as u8;
                // シフト先は同じ DPI 値を指し続けるように index を付け替える。
                let k = shift_dpi
                    .and_then(|sd| stages.iter().position(|s| s.x == sd))
                    .unwrap_or(0);
                p.data[l.shift_dpi] = k as u8;
                self.commit(p)?;
                self.pp.fcall(feat::ONBOARD_PROFILES, 0xC, &[a as u8]).ok();
                Ok(())
            }
            Change::Button { slot, action } => {
                if *slot >= ob.buttons as usize {
                    return Err(HubError::Invalid("ボタン番号".into()));
                }
                let (bytes, shift) = encode_button(action)?;
                let mut p = self.profile()?;
                let o = l.buttons + slot * 4;
                p.data[o..o + 4].copy_from_slice(&bytes);
                if let Some(dpi) = shift {
                    // シフト先は DPI ステージの index。一致するステージが無ければ
                    // 空きスロット (末尾) に追加する。
                    let mut stages = self.stages(&p.data);
                    let k = match stages.iter().position(|s| s.x == dpi) {
                        Some(k) => k,
                        None if stages.len() < DPI_SLOTS => {
                            stages.push(DpiStage {
                                x: dpi,
                                y: None,
                                color: Rgb { r: 0, g: 0, b: 0 },
                                enabled: true,
                            });
                            self.write_stages(&mut p.data, &stages);
                            stages.len() - 1
                        }
                        None => {
                            return Err(HubError::Invalid(
                                "DPI シフト先は既存の DPI ステージから選んでください".into(),
                            ))
                        }
                    };
                    p.data[l.shift_dpi] = k as u8;
                }
                self.commit(p)
            }
            Change::Setting { id, value } => {
                let mut p = self.profile()?;
                match id.as_str() {
                    "report_rate" => {
                        let hz = value.as_choice().max(1) as u32;
                        if !self.rates.contains(&hz) {
                            return Err(HubError::Invalid(format!("{hz}Hz")));
                        }
                        let b = if l.rate_code {
                            RATE_CODES.iter().position(|r| *r == hz).unwrap_or(3) as u8
                        } else {
                            (1000 / hz).clamp(1, 8) as u8
                        };
                        for o in l.rate {
                            p.data[*o] = b;
                        }
                    }
                    "angle_snap" => {
                        let o = l.angle_snap.ok_or(HubError::Unsupported("直線補正"))?;
                        p.data[o] = if value.as_bool() { 1 } else { 2 };
                    }
                    _ => return Err(HubError::Unsupported("この設定")),
                }
                self.commit(p)
            }
            Change::Lighting { area: 0, state } => {
                let o = l.leds.ok_or(HubError::Unsupported("ライト"))?;
                let mut p = self.profile()?;
                p.data[o..o + 11].copy_from_slice(&encode_led(state));
                self.commit(p)
            }
            _ => Err(HubError::Unsupported("この変更")),
        }
    }

    fn run_action(&self, id: &str, _file: Option<&std::path::Path>) -> HubResult<ActionOutcome> {
        match id {
            "reset_profile" => {
                let mut p = self.profile()?;
                p.data = self
                    .read_sector(0x0100 + p.index as u16)
                    .or_else(|_| self.read_sector(0x0101))?;
                self.commit(p)?;
                Ok(ActionOutcome {
                    message: Some("プロファイルを初期値に戻しました".into()),
                    reload: true,
                })
            }
            _ => Err(HubError::Unsupported("action")),
        }
    }

    fn battery(&self) -> HubResult<Battery> {
        let f = self.battery_feat.ok_or(HubError::Unsupported("battery"))?;
        match f {
            feat::UNIFIED_BATTERY => {
                // fn1 getStatus → [soc%, level, charging (0=放電 1=充電 2=低速充電 3=満充電), ext]
                let r = self.pp.fcall(f, 1, &[])?;
                Ok(Battery {
                    percent: Some(r[0].min(100)),
                    charging: matches!(r[2], 1 | 2),
                })
            }
            feat::BATTERY_VOLTAGE => {
                let r = self.pp.fcall(f, 0, &[])?;
                let mv = u16::from_be_bytes([r[0], r[1]]);
                Ok(Battery {
                    percent: Some(voltage_percent(mv)),
                    charging: r[2] & 0x80 != 0,
                })
            }
            _ => {
                let r = self.pp.fcall(f, 0, &[])?;
                Ok(Battery {
                    percent: (r[0] > 0).then_some(r[0].min(100)),
                    charging: matches!(r[2], 1..=3),
                })
            }
        }
    }
}

impl LogiDriver {
    fn current_dpi(&self) -> HubResult<u32> {
        match self.dpi_feat {
            Some(feat::EXTENDED_DPI) => {
                let r = self.pp.fcall(feat::EXTENDED_DPI, 5, &[0])?;
                Ok(u16::from_be_bytes([r[1], r[2]]) as u32)
            }
            Some(f) => {
                let r = self.pp.fcall(f, 2, &[0])?;
                Ok(u16::from_be_bytes([r[1], r[2]]) as u32)
            }
            None => Err(HubError::Unsupported("DPI")),
        }
    }

    fn set_current_dpi(&self, dpi: u32) -> HubResult<()> {
        let v = (snap_dpi(&self.dpi_ranges, dpi) as u16).to_be_bytes();
        match self.dpi_feat {
            Some(feat::EXTENDED_DPI) => {
                self.pp
                    .fcall(feat::EXTENDED_DPI, 6, &[0, v[0], v[1], v[0], v[1], 2])?;
            }
            Some(f) => {
                self.pp.fcall(f, 3, &[0, v[0], v[1]])?;
            }
            None => return Err(HubError::Unsupported("DPI")),
        }
        Ok(())
    }
}

fn utf16_name(b: &[u8]) -> String {
    let u: Vec<u16> = b
        .chunks(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|c| *c != 0 && *c != 0xFFFF)
        .collect();
    String::from_utf16_lossy(&u).trim().to_string()
}

/// 1 セル Li-ion の放電カーブ (Solaar の G 系近似表)。0x1001 (電圧のみ返す旧機種) 用。
pub fn voltage_percent(mv: u16) -> u8 {
    const T: &[(u16, u8)] = &[
        (4186, 100),
        (4067, 90),
        (3989, 80),
        (3922, 70),
        (3859, 60),
        (3811, 50),
        (3778, 40),
        (3751, 30),
        (3717, 20),
        (3671, 10),
        (3646, 5),
        (3579, 2),
        (3500, 0),
    ];
    if mv >= T[0].0 {
        return 100;
    }
    for w in T.windows(2) {
        let (hv, hp) = w[0];
        let (lv, lp) = w[1];
        if mv >= lv {
            let f = (mv - lv) as u32 * (hp - lp) as u32 / (hv - lv) as u32;
            return lp + f as u8;
        }
    }
    0
}

/// 別ハンドルで自発通知を読む: 0x8100 (本体ボタンでの DPI/プロファイル切替)・電池。
fn spawn_notify(
    path: std::ffi::CString,
    index: u8,
    id: DeviceId,
    ev: Arc<dyn Fn(HubEvent) + Send + Sync>,
    stop: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let Ok(api) = hidapi::HidApi::new() else { return };
        let Ok(dev) = api.open_path(&path) else { return };
        let pp = Hidpp::new(dev, index);
        let ob = pp.feature(feat::ONBOARD_PROFILES).ok().flatten();
        let bat = [
            feat::UNIFIED_BATTERY,
            feat::BATTERY_VOLTAGE,
            feat::BATTERY_STATUS,
        ]
        .into_iter()
        .find_map(|f| pp.feature(f).ok().flatten());
        let id2 = id.clone();
        pp.set_notify(Box::new(move |r: &[u8]| {
            if r.len() < 4 || r[1] != index {
                return;
            }
            // HID++1.0 接続状態通知 (0x41): 起床/スリープ。
            if r[0] == 0x10 && r[2] == 0x41 {
                ev(HubEvent::DevicesChanged);
                return;
            }
            if r[3] & 0x0F != 0 {
                return; // 誰かの要求への応答 (通知は swId=0)
            }
            if Some(r[2]) == ob {
                ev(HubEvent::StateChanged(id2.clone()));
            } else if Some(r[2]) == bat && r[0] == 0x11 {
                ev(HubEvent::Battery(
                    id2.clone(),
                    Battery {
                        percent: Some(r[4].min(100)),
                        charging: matches!(r[6], 1 | 2),
                    },
                ));
            }
        }));
        while !stop.load(Ordering::Relaxed) {
            pp.pump(500);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voltage_curve() {
        assert_eq!(voltage_percent(4200), 100);
        assert_eq!(voltage_percent(3811), 50);
        assert_eq!(voltage_percent(3400), 0);
    }

    #[test]
    fn button_roundtrip() {
        for a in [
            ButtonAction::Mouse(MouseFn::Left),
            ButtonAction::Mouse(MouseFn::Forward),
            ButtonAction::Mouse(MouseFn::ScrollLeft),
            ButtonAction::Dpi(DpiFn::Cycle),
            ButtonAction::Media(MediaFn::VolumeUp),
            ButtonAction::Key(KeyStroke { mods: 1, usage: 0x06 }),
            ButtonAction::Disabled,
        ] {
            let (b, _) = encode_button(&a).unwrap();
            assert_eq!(decode_button(&b, &[], 0), a);
        }
    }

    /// PRO X 2 実機 (fw MPM 32.05.B0029) の 0x2202 fn2 ページ 0/1 を連結したもの。
    #[test]
    fn dpi_list_pro_x2() {
        let page0 = [0x00, 0x64, 0xe0, 0x01, 0x00, 0xc8, 0xe0, 0x02, 0x01, 0xf4, 0xe0, 0x05, 0x03];
        let page1 = [0xe8, 0xe0, 0x0a, 0x07, 0xd0, 0xe0, 0x14, 0x13, 0x88, 0xe0, 0x32, 0x27, 0x10];
        let mut b = page0.to_vec();
        b.extend_from_slice(&page1);
        b.extend_from_slice(&[0, 0]);
        let r = parse_dpi_list(&b);
        assert_eq!(
            r,
            vec![
                (100, 200, 1),
                (200, 500, 2),
                (500, 1000, 5),
                (1000, 2000, 10),
                (2000, 5000, 20),
                (5000, 10000, 50),
            ]
        );
        assert_eq!(snap_dpi(&r, 2049), 2040);
        assert_eq!(snap_dpi(&r, 1600), 1600);
        assert_eq!(snap_dpi(&r, 50), 100);
    }

    #[test]
    fn led_roundtrip() {
        let mut st = decode_led(&[LED_BREATH, 10, 20, 30, 0x13, 0x88, 0, 80, 0, 0, 0]);
        assert_eq!(st.color, Rgb { r: 10, g: 20, b: 30 });
        assert_eq!(st.brightness, 80);
        st.on = true;
        let b = encode_led(&st);
        assert_eq!(&b[..4], &[LED_BREATH, 10, 20, 30]);
    }
}
