; picozorrousb.device: interrupt server entry (Exec server chain on
; INTB_PORTS or INTB_EXTER), backend `pz`. A1 = is_Data. Exec looks at the
; Z flag on return: set = not ours, go on with the next server; clear =
; handled.

	xdef	_pzu_isr
	xref	_pzu_isr_c

	section	code,code
_pzu_isr:
	jsr	_pzu_isr_c
	tst.l	d0
	rts
