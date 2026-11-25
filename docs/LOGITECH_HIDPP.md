# Logitech G (HID++ 2.0) — G HUB を使わない設定

`crates/devhub-hid/src/logitech.rs` (ドライバ) / `hidpp.rs` (トランスポート) の根拠。
すべて 2026-11-25 に実機で取得・検証した値。

## 検証機

| 項目 | 値 |
| --- | --- |
| マウス | PRO X SUPERLIGHT 2 (DeviceName = `PRO X 2`) |
| レシーバー | LIGHTSPEED Receiver `046D:C54D` (スロット 1) |
| FW | `MPM 32.05.B0029` (bootloader `BL1 71.05.B0029`) |
| HID++ | 4.2 |

## チャネル

- レシーバーの if2 に vendor `FF00` のコレクションが 2 つ: usage `01` = short (report 0x10, 7B)、
  usage `02` = long (report 0x11, 20B)。Windows では別ハンドルなので **long 側だけ開き、全要求を long で送る**。
- フレーム `[0x11, devIdx, featIdx, (fn<<4)|swId, params×16]`。スロット 1..6 に ping
  (Root fn1) して応答したものがマウス。有線直結は devIdx `0xFF`。
- 2.0 エラー: `[0x11, idx, 0xFF, featIdx, fn|sw, code]`。スリープ中のマウスは**応答なし (タイムアウト)**。
- swId=0 のフレームはデバイス自発の通知。G HUB と同時に開いても応答は swId で区別できる。

## feature 一覧 (PRO X 2)

```
0000 Root            0001 FeatureSet     0003 DeviceFwVersion  0005 DeviceName
1d4b WirelessStatus  0020 Reset          1004 UnifiedBattery   2250 AnalysisMode
2251 ?               2202 ExtendedDpi    8090 ModeStatus       80e0 ?
8061 ExtReportRate   8100 OnboardProf    8110 MouseButtonSpy   1500 ?
(以下 type=0x70 等の製造/隠し機能: 1801 1802 1803 1806 1817 1805 1830 1875 1861 1890 18a1 1e00 1e02 1e22 1602 1eb0 18b1)
```

LED (0x8070/0x8071)・ReprogControls (0x1B04)・旧 AdjustableDpi (0x2201)・ReportRate (0x8060) は**無い**。

## 実測した応答

| 要求 | 応答 | 意味 |
| --- | --- | --- |
| 1004 fn1 | `1f 04 00 00` | 残量 31%、level=good、放電中、外部給電なし |
| 2202 fn1 [0] | `00 05 0f` | センサー 0、DPI レベル 5、フラグ 0x0f (Y 分離等) |
| 2202 fn2 [0,0,page] | 連結すると `100 E001 200 E002 500 E005 1000 E00A 2000 E014 5000 E032 10000 …` | `0xE000|step` は区間刻み。最大 44000 |
| 2202 fn5 [0] | `00 0640 0640 0640 0640 02` | 現在 X/Y = 1600、既定 1600、LOD 2 |
| 8061 fn1 | `00 7f` | 対応レート bitmask: bit0=125Hz … bit6=8000Hz |
| 8061 fn2 | `03` | 現在 1000Hz |
| 8100 fn0 | `01 07 01 05 01 05 10 00ff 0a 04` | memory model 1 / **profile format 7** / macro fmt 1 / 5 プロファイル / 5 ボタン / 16 セクタ / セクタ 255B |
| 8100 fn2 | `01` | オンボードモード |
| 8100 fn4 | `00 01` | 現在プロファイル 1 |
| 8100 fn0xB | `02` | 現在 DPI スロット 2 |

## オンボードメモリ

- sector `0x0000` = ディレクトリ: 4B エントリ `[sector BE, enabled, 0xFF]`、`FFFF` 終端。
  実機: `0001 01 ff / 0002 00 ff / 0003 00 ff / 0004 00 ff / 0005 00 ff`。
- sector `0x0001..0x0005` = ユーザープロファイル、`0x0101` = ROM 既定 (ROM の CRC 欄は `ffff`)。
- 読み: fn5 `[sector BE, offset BE]` → 16B。末尾はオフセットを `size-16` に寄せて読む。
- 書き: fn6 `[sector BE, 0000, len BE]` → fn7 ×16B → fn8。末尾 2B は CRC-CCITT (init FFFF / poly 1021, BE)。
- 書いた後 fn3 `[00, profile]` で再選択すると本体が読み直す (DPI スロットは既定に戻るので fn0xC で戻す)。

### format 7 のバイト配置 (実機で確定)

| off | 内容 | 根拠 |
| --- | --- | --- |
| 0x00 | 有線時レポートレート (8061 コード) | 書き換えても無線の実測は不変 |
| 0x01 | **無線時レポートレート** | 3→2 で 8061 fn2 が 1000→500Hz に変化 |
| 0x02 | 既定 DPI スロット | プロファイル 1 = 2 (1600) / ROM = 0 (800) |
| 0x03 | DPI シフト先スロット | |
| 0x04 | DPI ×5: `X u16LE, Y u16LE, LOD` (0 = 空き) | 既定スロットを 1000 に書くと 2202 fn5 が 1000 |
| 0x1D | 不明 (0) | |
| 0x20 | 不明 (`00 ff 00 ff ff ..`) | G HUB のプロファイルでは 0x25 が 00 |
| 0x2C | `3c 00` / `2c 01` (60 / 300 — 省電力タイマーと推定) | 未変更 |
| 0x30 | ボタン割当 ×16 (4B) | `80 01 00 01` = 左 … `80 01 00 10` = 進む |
| 0x70 | G-Shift 割当 ×16 | |
| 0xA0 | プロファイル名 UTF-16LE ×24 | `PROFILE_NAME_DEFAULT` |
| 0xD0 | LED らしき 11B ブロック (LED 機能なし機でも存在) | 触らない |
| 0xFD | CRC | |

### format 1..3 (G PRO Wireless 等、libratbag と同配置 — 本機では未検証)

0x00 レート (ms) / 0x01 既定 DPI / 0x02 シフト DPI / 0x03 DPI u16LE ×5 / 0x0D 色 / 0x11 直線補正 /
0x20 ボタン ×16 / 0x60 G-Shift / 0xA0 名前 / 0xD0 LED ×2 (11B)。

## ボタン割当 (4B)

| バイト | 意味 |
| --- | --- |
| `80 01 hh ll` | マウスボタン bitmask BE (1 左 / 2 右 / 4 中 / 8 戻る / 0x10 進む) — 実機ダンプで確認 |
| `80 02 mm kk` | キーボード (修飾 / usage) |
| `80 03 hh ll` | コンシューマ usage BE |
| `90 ss 00 00` | 特殊: 01 チルト左 / 02 チルト右 / 03 DPI+ / 04 DPI− / 05 DPI 循環 / 07 DPI シフト / 0A プロファイル循環 — libratbag 由来、**本機では未検証** |
| `ff ff ff ff` | 無効 |

## 検証手順 (example)

```sh
cargo run -p devhub-hid --example logiprobe            # 読み取りのみ: feature・応答・全セクタのダンプ
cargo run -p devhub-hid --example logiwrite -- bk.bin  # 書き込み実験 (バックアップ→変更→実測→復元)
cargo run -p devhub-hid --example logie2e              # アプリと同じドライバ経路で往復確認
```

logie2e の結果 (2026-11-25): `report_rate=500` / `restored` / `dpi stages written` / `final state == original` すべて OK。
