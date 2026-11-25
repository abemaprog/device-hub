//! Logitech HID++ 2.0 トランスポート (LIGHTSPEED レシーバー / 有線)。
//!
//! - チャネル: vendor-defined `FF00` の long コレクション (usage 0x02, report 0x11, 20B)。
//!   Windows はコレクションごとに別ハンドルになるので long 側だけ開き、全要求を long で送る
//!   (HID++ 2.0 機は short 要求も long 要求も受ける)。
//! - フレーム: `[0x11, devIdx, featIdx, (func<<4)|swId, params×16]`。
//!   devIdx はレシーバー経由で 1..6 (スロット)、有線直結は 0xFF。
//! - 応答: 同じヘッダを返す。HID++2.0 エラーは `[0x11, idx, 0xFF, featIdx, func|sw, code]`、
//!   HID++1.0 エラー (無線スリープ等) は `[0x10, idx, 0x8F, featIdx, func|sw, code]`。
//! - swId が 0 のフレームはデバイス自発の通知。応答待ち中に来たものは `notify` へ渡す。
//!
//! 機能 (feature) の番号はデバイスごとに違うので、ID → index を Root (0x0000) で引いて
//! キャッシュする。

use devhub_core::backend::{HubError, HubResult};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const REPORT_SHORT: u8 = 0x10;
pub const REPORT_LONG: u8 = 0x11;
pub const LONG_LEN: usize = 20;

/// 主要 feature ID。
pub mod feat {
    pub const ROOT: u16 = 0x0000;
    pub const FEATURE_SET: u16 = 0x0001;
    pub const FW_VERSION: u16 = 0x0003;
    pub const DEVICE_NAME: u16 = 0x0005;
    pub const RESET: u16 = 0x0020;
    pub const BATTERY_STATUS: u16 = 0x1000;
    pub const BATTERY_VOLTAGE: u16 = 0x1001;
    pub const UNIFIED_BATTERY: u16 = 0x1004;
    pub const REPROG_CONTROLS_V4: u16 = 0x1B04;
    pub const WIRELESS_STATUS: u16 = 0x1D4B;
    pub const ADJUSTABLE_DPI: u16 = 0x2201;
    pub const EXTENDED_DPI: u16 = 0x2202;
    pub const REPORT_RATE: u16 = 0x8060;
    pub const EXT_REPORT_RATE: u16 = 0x8061;
    pub const COLOR_LED_EFFECTS: u16 = 0x8070;
    pub const RGB_EFFECTS: u16 = 0x8071;
    pub const ONBOARD_PROFILES: u16 = 0x8100;
    pub const MOUSE_BUTTON_SPY: u16 = 0x8110;
    pub const LATENCY_MONITOR: u16 = 0x8111;

    pub fn name(id: u16) -> &'static str {
        match id {
            0x0000 => "Root",
            0x0001 => "FeatureSet",
            0x0002 => "FeatureInfo",
            0x0003 => "DeviceFwVersion",
            0x0005 => "DeviceName",
            0x0007 => "DeviceFriendlyName",
            0x0020 => "Reset",
            0x00C2 => "DfuControlSigned",
            0x00C3 => "DfuControlSigned2",
            0x00D0 => "Dfu",
            0x1000 => "BatteryStatus",
            0x1001 => "BatteryVoltage",
            0x1004 => "UnifiedBattery",
            0x1300 => "LedControl",
            0x1801 => "ManufacturingMode",
            0x1802 => "DeviceReset",
            0x1803 => "GpioAccess",
            0x1805 => "OobState",
            0x1806 => "ConfigurableDeviceProperties",
            0x1811 => "?0x1811",
            0x1830 => "PowerModes",
            0x1890 => "RfTest",
            0x1891 => "RfTestBle",
            0x18A1 => "LedTest",
            0x18B0 => "MonitorMode",
            0x1B00 => "ReprogControls",
            0x1B04 => "ReprogControlsV4",
            0x1D4B => "WirelessDeviceStatus",
            0x1DF3 => "EquadDjDebugInfo",
            0x1E00 => "EnableHiddenFeatures",
            0x1E02 => "ManageDeactivatableFeatures",
            0x1E22 => "SpiDirectAccess",
            0x1EB0 => "TfDebug",
            0x1F1F => "FirmwareProperties",
            0x2201 => "AdjustableDpi",
            0x2202 => "ExtendedAdjustableDpi",
            0x2250 => "AnalysisMode",
            0x8060 => "ReportRate",
            0x8061 => "ExtendedReportRate",
            0x8070 => "ColorLedEffects",
            0x8071 => "RgbEffects",
            0x8081 => "PerKeyLighting",
            0x8090 => "ModeStatus",
            0x8100 => "OnboardProfiles",
            0x8110 => "MouseButtonSpy",
            0x8111 => "LatencyMonitoring",
            0x8120 => "GamingAttachments",
            0x8300 => "Sidetone",
            _ => "?",
        }
    }
}

/// HID++2.0 エラーコード → 文字列。
pub fn err_name(code: u8) -> &'static str {
    match code {
        0x01 => "Unknown",
        0x02 => "InvalidArgument",
        0x03 => "OutOfRange",
        0x04 => "HwError",
        0x05 => "LogitechInternal",
        0x06 => "InvalidFeatureIndex",
        0x07 => "InvalidFunctionId",
        0x08 => "Busy",
        0x09 => "Unsupported",
        _ => "?",
    }
}

/// HID++ 応答が返したエラー。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PpErr {
    /// HID++2.0 のエラーコード。
    V2(u8),
    /// HID++1.0 のエラー (0x8F)。無線スリープ/未接続は 0x09 (ResourceError) 等。
    V1(u8),
    Timeout,
}

pub type Notify = Box<dyn Fn(&[u8]) + Send + Sync>;

pub struct Hidpp {
    dev: Mutex<hidapi::HidDevice>,
    /// 0x01..0x06 (レシーバー経由) / 0xFF (有線)。
    pub index: u8,
    features: Mutex<HashMap<u16, u8>>,
    sw: AtomicU8,
    notify: Mutex<Option<Notify>>,
}

impl Hidpp {
    pub fn new(dev: hidapi::HidDevice, index: u8) -> Self {
        Self {
            dev: Mutex::new(dev),
            index,
            features: Mutex::new(HashMap::new()),
            sw: AtomicU8::new(0x8),
            notify: Mutex::new(None),
        }
    }

    pub fn set_notify(&self, f: Notify) {
        *self.notify.lock().unwrap_or_else(|p| p.into_inner()) = Some(f);
    }

    /// swId は 0x8..0xF を回す (0 は通知、G HUB 等の他アプリの応答と混ざりにくくする)。
    fn next_sw(&self) -> u8 {
        let s = self.sw.fetch_add(1, Ordering::Relaxed);
        0x8 | (s & 0x7)
    }

    /// 受信バッファに溜まった通知を捨てずに処理する (応答待ちの外から呼ぶ)。
    pub fn pump(&self, timeout_ms: i32) {
        let dev = self.dev.lock().unwrap_or_else(|p| p.into_inner());
        let mut buf = [0u8; 64];
        let mut wait = timeout_ms;
        while let Ok(n) = dev.read_timeout(&mut buf, wait) {
            if n == 0 {
                break;
            }
            self.dispatch(&buf[..n]);
            wait = 0;
        }
    }

    fn dispatch(&self, frame: &[u8]) {
        if let Some(f) = self.notify.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            f(frame);
        }
    }

    /// 生の要求 (`feat_idx` は index)。成功ならパラメータ 16B。
    pub fn call_raw(
        &self,
        feat_idx: u8,
        func: u8,
        params: &[u8],
        timeout: Duration,
    ) -> Result<[u8; 16], PpErr> {
        let sw = self.next_sw();
        let fsw = (func << 4) | sw;
        let mut w = [0u8; LONG_LEN];
        w[0] = REPORT_LONG;
        w[1] = self.index;
        w[2] = feat_idx;
        w[3] = fsw;
        let n = params.len().min(16);
        w[4..4 + n].copy_from_slice(&params[..n]);
        let dev = self.dev.lock().unwrap_or_else(|p| p.into_inner());
        // 古いフレームを先に吐き出す (応答の取り違え防止)。
        let mut buf = [0u8; 64];
        while let Ok(n) = dev.read_timeout(&mut buf, 0) {
            if n == 0 {
                break;
            }
            self.dispatch(&buf[..n]);
        }
        if dev.write(&w).is_err() {
            return Err(PpErr::Timeout);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(PpErr::Timeout);
            }
            let n = match dev.read_timeout(&mut buf, left.as_millis().max(1) as i32) {
                Ok(n) => n,
                Err(_) => return Err(PpErr::Timeout),
            };
            if n < 7 {
                continue;
            }
            let r = &buf[..n];
            if r[1] != self.index {
                self.dispatch(r);
                continue;
            }
            match (r[0], r[2]) {
                (REPORT_LONG, f) if f == feat_idx && r[3] == fsw => {
                    let mut out = [0u8; 16];
                    let m = (n - 4).min(16);
                    out[..m].copy_from_slice(&r[4..4 + m]);
                    return Ok(out);
                }
                (REPORT_LONG, 0xFF) if r[3] == feat_idx && r[4] == fsw => {
                    return Err(PpErr::V2(r[5]));
                }
                (REPORT_SHORT, 0x8F) if r[3] == feat_idx && r[4] == fsw => {
                    return Err(PpErr::V1(r[5]));
                }
                _ => self.dispatch(r),
            }
        }
    }

    /// 既定タイムアウト (無線往復 + 余裕) での要求。
    pub fn call(&self, feat_idx: u8, func: u8, params: &[u8]) -> HubResult<[u8; 16]> {
        let mut last = PpErr::Timeout;
        for _ in 0..3 {
            match self.call_raw(feat_idx, func, params, Duration::from_millis(600)) {
                Ok(r) => return Ok(r),
                Err(PpErr::V2(0x08)) => {
                    // Busy — 少し待って再送。
                    std::thread::sleep(Duration::from_millis(30));
                    last = PpErr::V2(0x08);
                }
                Err(PpErr::Timeout) => last = PpErr::Timeout,
                Err(e) => return Err(to_hub(e, feat_idx, func)),
            }
        }
        Err(to_hub(last, feat_idx, func))
    }

    /// プロトコル版 (Root fn1)。デバイス不在/スリープなら Err。
    pub fn ping(&self, timeout: Duration) -> Result<(u8, u8), PpErr> {
        let r = self.call_raw(0, 1, &[0, 0, 0x5A], timeout)?;
        Ok((r[0], r[1]))
    }

    /// feature ID → index (未対応は None)。
    pub fn feature(&self, id: u16) -> HubResult<Option<u8>> {
        if id == feat::ROOT {
            return Ok(Some(0));
        }
        if let Some(i) = self
            .features
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&id)
        {
            return Ok((*i != 0).then_some(*i));
        }
        let r = self.call(0, 0, &[(id >> 8) as u8, id as u8])?;
        self.features
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, r[0]);
        Ok((r[0] != 0).then_some(r[0]))
    }

    /// feature ID 指定の要求。未対応なら Unsupported。
    pub fn fcall(&self, id: u16, func: u8, params: &[u8]) -> HubResult<[u8; 16]> {
        let idx = self
            .feature(id)?
            .ok_or(HubError::Unsupported(feat::name(id)))?;
        self.call(idx, func, params)
    }

    /// 全 feature の一覧 `(id, type, version)`。
    pub fn list_features(&self) -> HubResult<Vec<(u16, u8, u8)>> {
        let fs = self
            .feature(feat::FEATURE_SET)?
            .ok_or(HubError::Unsupported("FeatureSet"))?;
        let count = self.call(fs, 0, &[])?[0];
        let mut out = vec![(0u16, 0u8, 0u8)];
        for i in 1..=count {
            let r = self.call(fs, 1, &[i])?;
            let id = u16::from_be_bytes([r[0], r[1]]);
            self.features
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(id, i);
            out.push((id, r[2], r[3]));
        }
        Ok(out)
    }

    /// DeviceName (0x0005)。
    pub fn device_name(&self) -> HubResult<String> {
        let len = self.fcall(feat::DEVICE_NAME, 0, &[])?[0] as usize;
        let mut s = Vec::new();
        while s.len() < len {
            let r = self.fcall(feat::DEVICE_NAME, 1, &[s.len() as u8])?;
            let take = (len - s.len()).min(16);
            s.extend_from_slice(&r[..take]);
        }
        Ok(String::from_utf8_lossy(&s)
            .trim_end_matches('\0')
            .trim()
            .to_string())
    }

    /// DeviceFwVersion (0x0003) の主ファーム (type 0) を "U1 12.01.B0010" 風に。
    pub fn firmware(&self) -> HubResult<Vec<(u8, String)>> {
        let n = self.fcall(feat::FW_VERSION, 0, &[])?[0];
        let mut out = vec![];
        for e in 0..n {
            let r = self.fcall(feat::FW_VERSION, 1, &[e])?;
            let prefix = String::from_utf8_lossy(&r[1..4]).trim().to_string();
            out.push((
                r[0] & 0x0F,
                format!(
                    "{prefix} {:02X}.{:02X}.B{:02X}{:02X}",
                    r[4], r[5], r[6], r[7]
                ),
            ));
        }
        Ok(out)
    }
}

fn to_hub(e: PpErr, feat_idx: u8, func: u8) -> HubError {
    match e {
        PpErr::Timeout => HubError::Protocol(format!(
            "HID++ 応答なし (feature#{feat_idx} fn{func})"
        )),
        PpErr::V1(_) => HubError::Disconnected,
        PpErr::V2(c) => HubError::Protocol(format!(
            "HID++ エラー {} (feature#{feat_idx} fn{func})",
            err_name(c)
        )),
    }
}

/// CRC-CCITT (init 0xFFFF, poly 0x1021)。オンボードプロファイルのセクタ末尾 2B (BE)。
pub fn crc_ccitt(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crc_known_vector() {
        // CRC-16/CCITT-FALSE("123456789") = 0x29B1
        assert_eq!(crc_ccitt(b"123456789"), 0x29B1);
    }
}
