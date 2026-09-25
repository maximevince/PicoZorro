; picozorro.device: interrupt server entry (Exec server chain on INTB_PORTS
; or INTB_EXTER). A1 = is_Data. Exec looks at the Z flag on return: set =
; not ours, go on with the next server; clear = handled.

	xdef	_pz_isr
	xref	_pz_isr_c

	section	code,code
_pz_isr:
	jsr	_pz_isr_c
	tst.l	d0
	rts
