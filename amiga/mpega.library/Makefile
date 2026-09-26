# mpega.library: MPEG audio decoded on the card (docs/REGISTERS-MPEG.md),
# and mpegtest, which drives it as a player does. bebbo gcc, common/gcc.mk.
include ../common/gcc.mk
.DEFAULT_GOAL := all

CFLAGS := -Iinclude
# romtag.o first: its LibNull is the first code in the file.
OBJS := romtag.o mpega.o card.o serlink.o bpclient.o
HDRS := mpega_lib.h include/libraries/mpega.h ../common/compiler.h ../common/serlink.h ../common/bplink.h ../common/bpclient.h build.flags

# DEBUG=1 or not: objects rebuild when it changes (build.flags records it).
build.flags: FORCE
	@echo "DEBUG=$(DEBUG)" | cmp -s - $@ || echo "DEBUG=$(DEBUG)" > $@
FORCE:

all: mpega.library mpegtest

mpega.library: $(OBJS)
	$(AMIGA_GCC) $(DEV_LDFLAGS) -o $@ $(OBJS) $(DEV_LIBS)

mpegtest: mpegtest.c include/libraries/mpega.h ../common/compiler.h
	$(AMIGA_GCC) $(PROG_CFLAGS) $(CFLAGS) $(PROG_LDFLAGS) -o $@ $<

romtag.o: romtag.c $(HDRS)
	$(AMIGA_GCC) $(DEV_CFLAGS_ROMTAG) $(CFLAGS) -c -o $@ $<

%.o: %.c $(HDRS)
	$(AMIGA_GCC) $(DEV_CFLAGS) $(CFLAGS) -c -o $@ $<

%.o: ../common/%.c $(HDRS)
	$(AMIGA_GCC) $(DEV_CFLAGS) $(CFLAGS) -c -o $@ $<

clean:
	rm -f $(OBJS) build.flags mpega.library mpegtest

.PHONY: all clean FORCE
