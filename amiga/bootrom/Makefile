# PicoZorro boot ROM: boot.bin, at most 256 bytes, which tools/mkboot.py
# puts into the firmware's boot image.
include ../common/gcc.mk
.DEFAULT_GOAL := all

all: boot.bin

boot.bin: boot.s
	$(AMIGA_VASM) -quiet -Fbin -m68000 -o $@ $<
	@test $$(stat -c %s $@) -le 256 || { echo "boot.bin is over 256 bytes"; rm -f $@; exit 1; }

clean:
	rm -f boot.bin

.PHONY: all clean
