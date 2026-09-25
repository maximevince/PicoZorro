# picozorro.device, SANA-II driver. bebbo gcc, common/gcc.mk;
# `make -C .. toolchain` fetches the rest. Plain 68000 code, runs on every
# Amiga.
include ../common/gcc.mk
.DEFAULT_GOAL := all

CFLAGS := -I$(SANA_INC)
# make DEBUG=1: log lines on the serial port
ifdef DEBUG
CFLAGS += -DPZ_DEBUG
endif
# romtag.o first: its LibNull is the first code in the file.
OBJS := romtag.o device.o backend_uae.o backend_loop.o backend_pz.o backend_bp.o serlink.o bpclient.o isr.o
HDRS := device.h ../common/compiler.h ../common/serlink.h ../common/bplink.h ../common/bpclient.h build.flags

# DEBUG=1 or not: objects rebuild when it changes (build.flags records it).
build.flags: FORCE
	@echo "DEBUG=$(DEBUG)" | cmp -s - $@ || echo "DEBUG=$(DEBUG)" > $@
FORCE:

all: picozorro.device s2test

# Command-level test tool (run with ENV:PZNET = loop), a normal program.
s2test: s2test.c ../common/compiler.h
	$(AMIGA_GCC) $(PROG_CFLAGS) $(CFLAGS) $(PROG_LDFLAGS) -o $@ $<

picozorro.device: $(OBJS)
	$(AMIGA_GCC) $(DEV_LDFLAGS) -o $@ $(OBJS) $(DEV_LIBS)

romtag.o: romtag.c $(HDRS)
	$(AMIGA_GCC) $(DEV_CFLAGS_ROMTAG) $(CFLAGS) -c -o $@ $<

%.o: %.c $(HDRS)
	$(AMIGA_GCC) $(DEV_CFLAGS) $(CFLAGS) -c -o $@ $<

%.o: ../common/%.c $(HDRS)
	$(AMIGA_GCC) $(DEV_CFLAGS) $(CFLAGS) -c -o $@ $<

isr.o: isr.s
	$(AMIGA_VASM) -quiet -Fhunk -m68000 -o $@ $<

clean:
	rm -f $(OBJS) build.flags picozorro.device s2test

.PHONY: all clean FORCE
