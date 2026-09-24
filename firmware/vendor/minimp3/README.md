# minimp3 (vendored)

lieff/minimp3 `minimp3.h` at ea99364f61c14656440e8d77e9c233ccf3124633,
unchanged, CC0 (`LICENSE`). MPEG-1/2/2.5 layers I, II and III.

`minimp3.c` is the one translation unit: `MINIMP3_IMPLEMENTATION`,
`MINIMP3_NO_SIMD`, 16-bit output. pz-app's build.rs compiles it with
arm-none-eabi-gcc (`-O2 -ffp-contract=off`) when a feature needs it.
