# Electrical facts used by the design (with sources)

Only figures read from the named documents. "NOT FOUND" means the document
is silent; do not fill in.

## RP2350B (datasheet build 2025-07-29, d126e9e; HW design guide Release 3; PCN 28)

| Fact | Value | Where |
|---|---|---|
| 5 V tolerant (FT) pins | GPIO0-39, RUN, SWCLK, SWDIO | Tables 1427-1431, p.1335-1338 |
| NOT 5 V tolerant | GPIO40-47 (ADC), QSPI, USB, XIN/XOUT | same; ADC note p.442/p.1068 |
| FT pin abs max, IOVDD = 3.3 V | -0.5 .. 5.5 V, "IOVDD must be present" | Table 1433 p.1338 |
| FT pin abs max, IOVDD = 0 V | -0.5 .. **3.63 V** | Table 1433 |
| FT VIH operating max | 5.5 V (= abs max, zero margin) | Table 1436 p.1339 |
| VIH / VIL at 3.3 V | 2.0 V / 0.8 V, hysteresis >= 0.2 V | Table 1436 |
| VOH / VOL | >= 2.62 V / <= 0.5 V at the selected 2/4/8/12 mA | Table 1436 |
| Total GPIO source / sink | 100 mA / 100 mA | Table 1436 |
| Pad capacitance, slew numbers, per-pin abs max current | NOT FOUND | |
| Leakage specifically at 5 V | NOT FOUND (IIN <= 1 uA, no test voltage given) | |
| Reset state of GPIO pads | IE=0, ISO=1, pull-down enabled, 4 mA | Table 853 p.787 |
| Erratum E9 (input leakage) | A2 only; fixed in A3 silicon; A4 = production | App. E p.1366, App. C p.1354, PCN 28 |
| Stepping check | CHIP_ID.REVISION: A2=2, A3=3, A4=8; marking `RP2350B0A4` | App. C, 14.4 |
| PIO | 3 blocks x 4 SM, 32 instr/block, 1 instr per clk_sys | 11.2 p.876-879 |
| PIO pin window | 32 pins, GPIOBASE = 0 or 16 only, per block | p.877, p.956 |
| Input synchroniser | 2 clk_sys cycles; per-pin bypass INPUT_SYNC_BYPASS | 11.5.6.3 p.913 |
| Cross-block PIO IRQ flags | visible next cycle, no penalty | p.878 |
| clk_sys max officially supported | 150 MHz (nothing higher documented) | p.13, p.519 |
| Boot time to user code | NOT FOUND (no figure anywhere) | |

Raspberry Pi news post 2025-07-29: "keep IOVDD powered when 5V is applied to
any GPIO pad, otherwise the pad will be damaged."

## Logic parts (TI datasheets, VCC = 3.3 V +/- 0.3 V, -40..85 C)

| Part | Fact | Value |
|---|---|---|
| SN74LVC245A | I/O tolerate 5.5 V; Ioff at VCC = 0, VI/VO = 5.5 V | +/-10 uA max |
| SN74LVC245A | tpd A<->B | 1.5 / 3.8 typ / 6.3 ns |
| SN74LVC245A | ten / tdis (OE) | <= 8.5 / <= 7.5 ns |
| SN74LVC245A | VOH @ -24 mA (3 V) / VOL @ 24 mA | >= 2.2 V / <= 0.55 V |
| SN74LVC245A | "OE should be tied to VCC through a pullup" for Hi-Z at power up | section 8.3 |
| SN74LVC16245A | same family features, Ioff +/-10 uA | |
| SN74LVC1G07 (open drain) | I/O tolerate 5.5 V, Ioff +/-10 uA | |
| SN74LVC1G07 | tpd A->Y | 1.5 / 4.2 ns max |
| SN74LVC1G07 | VOL @ 16 mA / 24 mA (VCC 3 V) | <= 0.4 V / <= 0.55 V |

## Denise load figures (from the Denise schematic)

- XRDY pull-up 470 R to 5 V -> driver sinks 5/470 = 10.6 mA, idles at 5 V.
- /CFGOUT1 pull-down 1 k, receiver SN74LS32 (VIH 2.0 V).
