# Small diagnostic programs for an Amiga with the card, bebbo gcc.
include ../common/gcc.mk
PROGS := devinfo memdump memcheck pzbench pzload
all: $(PROGS)
%: %.c
	$(AMIGA_GCC) $(PROG_CFLAGS) $(PROG_LDFLAGS) -o $@ $<
clean:
	rm -f $(PROGS)
.PHONY: all clean
