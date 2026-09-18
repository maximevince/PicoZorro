# Zorro II card edge: orientation and geometry

A mirrored edge connector puts +5 V on signal pins, so this is written down
with its evidence.

## Orientation
Viewed from the **component side with the fingers pointing down**:
- **pin 2 is the rightmost finger, pin 100 the leftmost**; even pins are on
  the component side;
- odd pins are on the solder side, pin 1 directly behind pin 2;
- the right end is the rear/bracket end of the machine (on the Denise: the
  rear I/O end, where Enterlogic's A-NIC card has its RJ45).

Evidence:
1. Commodore A500/A2000 Technical Reference Manual, appendix A, drawing A-5
   "Amiga 2000 Form Factor" (printed rotated on the page): labelled
   COMPONENT SIDE, it shows PIN 2 165.735 mm from the bracket-end edge and
   PIN 100 further away; the bracket end is on the right when the drawing is
   turned upright.
2. The front copper artwork of Enterlogic's A-NIC card: the two rightmost
   component-side fingers tie straight into the ground pour (pins 2, 4 =
   GND) and the third has a decoupling capacitor next to it (pin 6 = +5 V).
   With pin 100 on the right that pattern would be GND, n/c, n/c.

## Geometry (TRM drawing A-5, mm)
- 50 fingers per side, pitch 2.54, row length 49 x 2.54 = 124.46
- tongue width 129.26 +/- 0.1 (so 2.40 from tongue edge to first/last finger centre)
- tongue depth 7.62; 1.5 x 45 deg corner chamfers; 0.5 x 45 deg edge bevel
- shoulder radius R1.5
- finger width: the drawing shows 1.6; the footprint uses 1.65 mm from the
  mating-board drawing of the TE 2-5530843-2 connector (.065 +/- .002 in),
  and a 1.6 mm board (the EDAC 745 series accepts 1.37 to 1.78 mm). Sources
  and the full dimension table: `lib/README.md`.

## Card outline
Enterlogic's A-NIC, a mini-Zorro card known to fit the Denise, is 155.5 x
54.3 mm, bracketless, with its I/O at the right (rear) end. The PicoZorro
One TH keeps the 155.5 mm length and the I/O end; its body is taller
(58.3 mm above the shoulder, 65.92 mm with the tongue).
