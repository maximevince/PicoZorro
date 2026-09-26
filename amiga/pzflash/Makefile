# pzflash: firmware update from the Amiga (docs/UPDATE.md). bebbo gcc,
# common/gcc.mk, a normal program.
include ../common/gcc.mk

OBJS := pzflash.o serlink.o bpclient.o
HDRS := ../common/bplink.h ../common/bpclient.h ../common/serlink.h

pzflash: $(OBJS)
	$(AMIGA_GCC) $(PROG_LDFLAGS) -o $@ $(OBJS)

%.o: %.c $(HDRS)
	$(AMIGA_GCC) $(PROG_CFLAGS) -c -o $@ $<

%.o: ../common/%.c $(HDRS)
	$(AMIGA_GCC) $(PROG_CFLAGS) -c -o $@ $<

clean:
	rm -f $(OBJS) pzflash

.PHONY: clean
