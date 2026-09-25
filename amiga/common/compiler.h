/*
 * Register parameters for the AmigaOS calling convention. The tree builds
 * with bebbo's m68k-amigaos-gcc; vbcc and Bartman's m68k-amiga-elf-gcc
 * with jbilander's regparm patch work too.
 *
 * REGARG on definitions and on everything whose address leaves the
 * driver (device vectors, hooks, the ISR): the builds use -mregparm, so
 * a function without it takes its arguments in registers of gcc's choice.
 */
#ifndef PZ_COMPILER_H
#define PZ_COMPILER_H

#include <exec/types.h>

#ifdef __VBCC__
#define REGARG(decl, r) __reg(#r) decl
#else
#define REGARG(decl, r) decl __asm(#r)
#endif

/* Bartman's gcc takes register parameters on function definitions only:
 * prototypes and function pointer types go without them (REGPROTO), and
 * calls through such a pointer use CALL_COPY. */
#if defined(__GNUC__) && defined(__ELF__)
#define REGPROTO(decl, r) decl
#define CALL_COPY(f, to, from, n) pz_call_copy((APTR)(f), (to), (from), (n))
static inline BOOL pz_call_copy(APTR f, APTR to, APTR from, ULONG n)
{
    register ULONG d0 __asm("d0") = n;
    register APTR a0 __asm("a0") = to;
    register APTR a1 __asm("a1") = from;
    register APTR a2 __asm("a2") = f;
    __asm volatile("jsr (%%a2)" : "+r"(d0), "+r"(a0), "+r"(a1) : "r"(a2) : "d1", "cc", "memory");
    return (BOOL)d0;
}
#else
#define REGPROTO(decl, r) REGARG(decl, r)
#define CALL_COPY(f, to, from, n) (f)((to), (from), (n))
#endif

#endif
