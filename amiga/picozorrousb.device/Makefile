# picozorrousb.device, Poseidon hardware driver (docs/REGISTERS-USB.md).
# bebbo gcc, common/gcc.mk; `make -C .. toolchain` fetches the rest,
# Poseidon's headers included (rondoval/poseidon-backport).
# Plain 68000 code; Poseidon itself wants a 68020.
include ../common/gcc.mk
.DEFAULT_GOAL := all

CFLAGS := -I$(PSD_INC)
# make DEBUG=1: log lines on the serial port
ifdef DEBUG
CFLAGS += -DPZU_DEBUG
endif
# romtag.o first: its LibNull is the first code in the file.
OBJS := romtag.o device.o backend_uae.o backend_ser.o backend_bp.o backend_pz.o serlink.o bpclient.o isr.o
HDRS := device.h ../common/compiler.h ../common/serlink.h ../common/bplink.h ../common/bpclient.h build.flags

# DEBUG=1 or not: objects rebuild when it changes (build.flags records it).
build.flags: FORCE
	@echo "DEBUG=$(DEBUG)" | cmp -s - $@ || echo "DEBUG=$(DEBUG)" > $@
FORCE:

all: picozorrousb.device usbtest stortest

# Command-level test tool, a normal program.
usbtest: usbtest.c
	$(AMIGA_GCC) $(PROG_CFLAGS) $(CFLAGS) $(PROG_LDFLAGS) -o $@ $<

# USB stick check through Poseidon's usbscsi.device.
stortest: stortest.c
	$(AMIGA_GCC) $(PROG_CFLAGS) $(PROG_LDFLAGS) -o $@ $<

picozorrousb.device: $(OBJS)
	$(AMIGA_GCC) $(DEV_LDFLAGS) -o $@ $(OBJS) $(DEV_LIBS)

romtag.o: romtag.c $(HDRS)
	$(AMIGA_GCC) $(DEV_CFLAGS_ROMTAG) $(CFLAGS) -c -o $@ $<

isr.o: isr.s
	$(AMIGA_VASM) -quiet -Fhunk -m68000 -o $@ $<

%.o: %.c $(HDRS)
	$(AMIGA_GCC) $(DEV_CFLAGS) $(CFLAGS) -c -o $@ $<

%.o: ../common/%.c $(HDRS)
	$(AMIGA_GCC) $(DEV_CFLAGS) $(CFLAGS) -c -o $@ $<

clean:
	rm -f $(OBJS) build.flags picozorrousb.device usbtest stortest

.PHONY: all clean FORCE
