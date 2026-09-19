# PicoZorro KiCad library

| File | What |
|---|---|
| `PicoZorro.kicad_sym` | symbol `Zorro_II_Edge_100` (ref `J`, footprint `PicoZorro:Zorro_II_Edge_100`) |
| `PicoZorro.pretty/Zorro_II_Edge_100.kicad_mod` | Zorro II card edge, 2 x 50 gold fingers (KiCad 10) |

Everything else on the PicoZorro One TH comes from the standard KiCad
libraries. The modules (Core2350B, W5500 Lite, microSD and DAC breakouts)
sit on standard 2.54 mm pin-header footprints placed in the board.

The project's `fp-lib-table` and `sym-lib-table` point here as
`${KIPRJMOD}/../lib`.

## Orientation (safety critical)

Seen from **F.Cu with the fingers pointing down (+Y)**: **pad 2 is the rightmost
finger (X = +62.23), pad 100 the leftmost (X = -62.23)**. Even pads 2..100 are
on F.Cu (component side), odd pads 1..99 on B.Cu directly behind: pad 1 shares
X with pad 2, pad 99 with pad 100. Evidence: `../EDGE_CONNECTOR.md`.
Silk "2"/"100" (front) and "1"/"99" (back) mark the ends.

Origin: X = centre of the finger row, Y = 0 at the tongue tip (card bottom
edge). The tongue is in -Y. Snap the origin onto the bottom edge of the outline.

## Dimension sources

- [CBM] Commodore A500/A2000 Technical Reference Manual, appendix A, drawing
  A-5 "Amiga 2000 Form Factor": main view, detail X and the bevel section.
- [TE] TE Connectivity customer drawing C-5530843 rev G5 "Connector assembly,
  Standard Edge II, .100 centerline", 6 sheets, inches. 2-5530843-2 is the
  50-position row of the table on sheet 4.
  https://www.te.com/commerce/DocumentDelivery/DDEController?Action=srchrtrv&DocNm=5530843&DocType=Customer+Drawing&DocLang=English
- [EDAC] EDAC 745 Series Press-Fit Card Edge Connectors, English Ordering
  Guide, 2 pages (example part number on p1 is 745-100-520-206).
  https://files.edac.net/edac/content/series/og/English/EDAC%20745%20Series%20Pressfit%20Card%20Edge%20Connectors%20English%20Ordering%20Guide.pdf
- [JLC] JLCPCB help "JLCPCB Gold fingers"
  (https://jlcpcb.com/help/article/jlcpcb-gold-fingers) and the JLCPCB blog
  articles "PCB Card Edge Connectors: Design Essentials & Gold Fingers"
  (https://jlcpcb.com/blog/pcb-card-edge-connectors-design-gold-fingers-manufacturing)
  and "Beveled Edges for Better Card Edge Connectors"
  (https://jlcpcb.com/blog/beveled-edges-card-edge-connectors).

| Dimension | Value used | Source |
|---|---|---|
| Positions | 2 x 50 | [CBM] A-5 "PIN 2 ... PIN 100", "49 x 2.54"; [EDAC] p2 table row 50/100 |
| Pitch | 2.54 mm | [CBM] A-5 "49 x 2.54 = 124.46", detail X "2.54"; [TE] sh6 "X spaces at .100"; [EDAC] p1 |
| Row span pad 2..pad 100 | 124.46 mm | [CBM] A-5; [TE] sh4 D = 4.900 in; [EDAC] p2 "E" = 124.46 |
| Tongue width | 129.26 mm (+/-0.1) | [CBM] A-5 "129.26 +/-0.1" |
| Tongue edge to first/last finger centre | 2.40 mm | derived: (129.26 - 124.46)/2, fingers centred. [CBM] detail X prints a rounded "2.5"; [TE] sh6 gives .093 in = 2.36 |
| Tongue depth | 7.62 mm | [CBM] A-5 "7.62" |
| Lower corner chamfer | 1.5 mm x 45 deg | [CBM] detail X "1.5 x 45" |
| Shoulder radius | R1.5 | [CBM] detail X "R1.5" |
| Edge bevel (fab note only) | 0.5 x 45 deg | [CBM] section "0.5 x 45 deg BEVEL". [TE] sh6 recommends a longer .064 in x 20 deg lead-in; JLCPCB offers 30 or 45 deg only |
| **Finger width** | **1.65 mm** | [TE] sh6 "Recommended mating board edge configuration": .065 +/-.002 in = 1.651 (1.600..1.702). [CBM] detail X shows 1.6, the low end of that range. [EDAC] gives no pad width |
| Finger top (min) | >= 5.08 mm from card edge | [TE] sh6 ".200 MIN" from datum B (card edge) |
| Finger top (used) | 7.62 mm = top of tongue | [CBM] detail X (fingers end at the shoulder line) |
| **Finger start** | **0.60 mm from card edge** | [JLC] blog: "a 30 degree bevel requires a 0.6 mm gap between the pad and the edge of a 1.6mm PCB"; also clears the [CBM] 0.5 x 45 deg bevel |
| Finger pad | 1.65 x 7.02 mm, centre Y = -4.11 | derived from the three rows above |
| Finger gap | 0.89 mm | derived: 2.54 - 1.65 |
| **Board thickness** | **1.6 mm** | [EDAC] p1 "Accepts 0.062in (1.57mm) nominal", p2 "card slot accepts .054 (1.37) to .070 (1.78)"; [TE] sh1/sh3 "card slot accepts .070-.054", sh6 mating board .065 +/-.010 in |
| Contact point, TE | 3.94 mm (.155 in) above slot floor; slot .295 in = 7.49 mm deep | [TE] sh1 section Z-Z. A 7.62 tongue bottoms in the slot, so contact is ~3.9 mm from the tip |
| Contact point, EDAC code 520 / insulator 06 | 6.60 mm (.260 in) above slot floor; slot .420 in = 10.67 mm deep | [EDAC] p2 sections Y-Y. The tongue (7.62) is shorter than the slot, so the card seats on its shoulders and contact is ~3.55 mm from the tip |
| Mask | one window per side over the whole tongue, no paste | [JLC] help: "The gold finger area must have the solder mask fully opened" |
| Min board size for gold fingers / bevel | 50 x 50 mm | [JLC] help |

### Choices

- **Finger start 0.60 mm, not plated to the edge.** Both mating connectors touch
  the finger 3.5-3.9 mm from the tip, so nothing is lost, and the bevel
  (JLCPCB 30/45 deg; Commodore 0.5 x 45 deg) never cuts copper, which would
  raise burrs and lift finger ends. If hard gold (electroplated) is ordered the
  fab adds and removes its own plating tie bars; do not draw them.
- **Finger width 1.65 mm** follows the TE drawing; Commodore's 1.6 is the lower
  limit of TE's tolerance. Either is electrically safe at 2.54 pitch.
- **Rectangular finger ends.** TE draws rectangles; Commodore draws rounded tops.
  Rectangles keep the trace exit simple.
- **Keepout rule area** (no copper pour, no vias; tracks and pads allowed) over
  the tongue on F/B/In1-In4: the mask is open there, so a zone fill between
  fingers would be bare copper under the connector contacts, and inner copper
  at the tip would be exposed by the bevel.
- **Courtyards** F and B over the tongue so nothing gets placed on it.
- Tongue outline is on **Dwgs.User** (copies on F.Fab/B.Fab), never Edge.Cuts.
  A thin Cmts.User line at Y = -0.5 shows where the 0.5 mm bevel ends.

### Things to know when drawing the outline

- [EDAC] p2 recommends a daughter-board tab of "D" - 0.30 mm +0/-0.38 =
  128.86..129.24 mm for the 129.54 mm slot. Commodore's 129.26 +/-0.1 is at or
  just above that; it still clears the slot by 0.28 mm nominal and is what every
  Zorro II card uses. [TE] sh4/sh6 gives Z = 5.086 in = 129.18 mm.
- No polarizing key slot is cut in a Zorro II tongue ([CBM] A-5 shows none).

## Symbol

Single unit, tall rectangle, odd pins left, even pins right, numeric order top
to bottom at 2.54 mm, as in the Amiga manual tables. Active-low pins are named
`~{NAME}`. `power_in`: GND, +5V, +12V, -5V, -12V. Everything else `passive`,
including pin 91 `GND(SENSEZ3)` (a sense line on Zorro III backplanes; the
card ties it to GND) and NC97/NC98. No hidden or stacked pins. `in_bom no`,
`in_pos_files no` to match the footprint attributes. The +12V/-5V/-12V
`power_in` pins need a PWR_FLAG or a no-connect if unused.

KLC checker (`kicad-library-utils/klc-check`) results, all deliberate:
S3.1 (50 rows on a 2.54 grid cannot be centred, off by 1.27 mm), S4.2 (pins in
connector order, not grouped by function), F9.3 (no 3D model for a card edge).

## Not verified

- No individual EDAC drawing for 745-100-520-206 was found online; the series
  ordering guide was used. EDAC publishes no mating pad width, so that number
  rests on the TE drawing alone.
- The 0.6 mm pad-to-edge figure is from a JLCPCB blog article and is stated for
  the 30 deg bevel; JLCPCB's help page only says "sufficient clearance". Confirm
  bevel angle and depth in the order notes.
- Where the card actually seats in the Denise's EDAC connectors (shoulder on
  housing vs. held by the case) is not measured; the contact-height figures
  above are from the drawings.
