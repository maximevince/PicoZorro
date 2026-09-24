/* The minimp3 implementation for the firmware (pz-app build.rs): header
 * only upstream, one translation unit here. Layers I, II and III; 16-bit
 * output; no SIMD. */
#define MINIMP3_IMPLEMENTATION
#define MINIMP3_NO_SIMD
#include "minimp3.h"
