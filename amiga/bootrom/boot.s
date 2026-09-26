; PicoZorro boot ROM.
;
; The firmware serves this at window offset 0 in ROM mode; the Autoconfig
; ROM says DIAGVALID with er_InitDiagVec $0100 (the window aliases every
; 256 bytes). Kickstart (1.3 .. 3.2.2) copies da_Size bytes to RAM and
; calls DiagPoint, which relocates the romtag in the copy; at romboot time
; Kickstart finds the romtag and InitResidents it, and `init` below pulls
; the modules (picozorro.device, picozorrousb.device, mpega.library) from
; the card's boot stream, already relocated by the firmware:
;
;   "PB", then per module: hunks n, n x (bytes.l, MEMF.l)  -> AllocMem each,
;   write the address to BOOT_ADDR; then n x (address.l, words.l, words),
;   the romtag address.l -> InitResident. Hunks 0: the end.
;
; Assemble: vasmm68k_mot -Fbin -m68000 (amiga/bootrom/Makefile).

AllocMem     = -198
InitResident = -102
CacheClearU  = -636
LIB_VERSION  = 20

BOOT_CTRL    = $80      ; W: 1 = ROM mode off, the stream from the start
BOOT_ADDR    = $82      ; W: hunk address, long ($82 high, $84 low)
BOOT_STAT    = $86      ; R: bit 15 busy
BOOT_DATA    = $88      ; R: the stream ($88 and $8A: move.l works)
WAIT_LOOPS   = 400000   ; ~1-3 s of polling before giving up

        section boot,code
da:     dc.b    $90             ; da_Config: DAC_WORDWIDE | DAC_CONFIGTIME
        dc.b    0               ; da_Flags
        dc.w    romend-da       ; da_Size
        dc.w    diag-da         ; da_DiagPoint
        dc.w    bootpt-da       ; da_BootPoint (Kickstart wants it non-zero)
        dc.w    name-da         ; da_Name
        dc.w    0,0
tag:    dc.w    $4afc           ; RTC_MATCHWORD
        dc.l    tag-da          ; rt_MatchTag: relocated by diag
        dc.l    romend-da       ; rt_EndSkip
        dc.b    0               ; rt_Flags: InitResident calls rt_Init
        dc.b    1               ; rt_Version
        dc.b    0               ; rt_Type
        dc.b    0               ; rt_Pri
        dc.l    name-da         ; rt_Name
        dc.l    name-da         ; rt_IdString
        dc.l    init-da         ; rt_Init
base:   dc.l    0               ; the board, stored by diag

; DiagPoint: a0 = board, a2 = the RAM copy. Relocate the romtag, remember
; the board, keep the copy (d0 != 0).
diag:   lea     tag+2-da(a2),a1
        move.l  a2,d0
        add.l   d0,(a1)+        ; rt_MatchTag
        add.l   d0,(a1)+        ; rt_EndSkip
        addq.l  #4,a1
        add.l   d0,(a1)+        ; rt_Name
        add.l   d0,(a1)+        ; rt_IdString
        add.l   d0,(a1)+        ; rt_Init
        move.l  a0,(a1)         ; base
bootpt: moveq   #1,d0
        rts

; rt_Init, from InitResident at romboot: a6 = ExecBase.
init:   movem.l d2-d7/a2-a5,-(sp)
        move.l  base(pc),a5
        lea     BOOT_DATA(a5),a4
        lea     BOOT_ADDR(a5),a3
        move.w  #1,BOOT_CTRL(a5)
        bsr.s   wait
        bmi.s   done
        cmp.w   #$5042,(a4)     ; "PB"
        bne.s   done
mod:    move.w  (a4),d7         ; hunks in the next module, 0 = end
        beq.s   done
        subq.w  #1,d7
        move.w  d7,d6
alloc:  move.l  (a4),d0         ; bytes
        move.l  (a4),d1         ; MEMF flags
        jsr     AllocMem(a6)
        move.l  d0,(a3)         ; the firmware relocates against it
        beq.s   done
        dbra    d7,alloc
        bsr.s   wait
        bmi.s   done
hunk:   move.l  (a4),a0         ; hunk address
        move.l  (a4),d0         ; words
        bra.s   in
copy:   move.w  (a4),(a0)+
in:     subq.l  #1,d0
        bcc.s   copy
        dbra    d6,hunk
        cmp.w   #37,LIB_VERSION(a6)
        bcs.s   nocache
        jsr     CacheClearU(a6)
nocache:
        move.l  (a4),a1         ; the module's romtag
        moveq   #0,d1
        jsr     InitResident(a6)
        bra.s   mod
done:   movem.l (sp)+,d2-d7/a2-a5
        moveq   #0,d0
        rts

; Until BOOT_STAT is not busy; N set: gave up.
wait:   move.l  #WAIT_LOOPS,d2
wloop:  move.w  BOOT_STAT(a5),d0
        bpl.s   wdone
        subq.l  #1,d2
        bne.s   wloop
        moveq   #-1,d0
wdone:  rts

name:   dc.b    "PicoZorro boot",0
        even
romend:
