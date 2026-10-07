# The Amiga side of PicoZorro: pztest here; the drivers, mpega.library,
# pzflash, the boot ROM and the diagnostic tools in their directories.
# Built with bebbo's m68k-amigaos-gcc from a docker image pinned by digest
# (common/gcc.mk). `make toolchain` pulls the image and fetches what it
# lacks into .toolchain/ (not in the repository):
#   NDK 3.2 R4 (Hyperion)  http://aminet.net/dev/misc/NDK3.2.lha
#                          (SANA-II and Roadshow headers)
#   Poseidon headers       github.com/rondoval/poseidon-backport, pinned
include common/gcc.mk

# lha, or 7-Zip where lha is missing (it reads .lha too, e.g. on GitHub's runners).
LHA_X := $(if $(shell command -v lha 2>/dev/null),lha xq,7z x -y -bd)

POSEIDON_REV := 754216c5f91c00fd189da2b6ef526d6c622c25b2
POSEIDON_HDRS := devices/usb.h devices/usb_hub.h devices/usbhardware.h devices/usbhcd_common.h libraries/poseidon.h

all: pztest
	$(MAKE) -C picozorro.device
	$(MAKE) -C picozorrousb.device
	$(MAKE) -C mpega.library
	$(MAKE) -C pzflash
	$(MAKE) -C bootrom
	$(MAKE) -C pztools

pztest: pztest.c
	$(AMIGA_GCC) $(PROG_CFLAGS) $(PROG_LDFLAGS) -o $@ $<

toolchain:
	docker pull $(BEBBO_IMAGE)
	mkdir -p $(TC)/NDK3.2
	[ -d $(TC)/NDK3.2/SANA+RoadshowTCP-IP ] || (cd $(TC) && curl -sLO http://aminet.net/dev/misc/NDK3.2.lha \
	  && cd NDK3.2 && $(LHA_X) ../NDK3.2.lha)
	$(MAKE) poseidon-headers

poseidon-headers:
	for f in $(POSEIDON_HDRS); do mkdir -p $(PSD_INC)/$$(dirname $$f) && curl -sfL -o $(PSD_INC)/$$f \
	  https://raw.githubusercontent.com/rondoval/poseidon-backport/$(POSEIDON_REV)/include/$$f || exit 1; done

clean:
	rm -f pztest
	$(MAKE) -C picozorro.device clean
	$(MAKE) -C picozorrousb.device clean
	$(MAKE) -C mpega.library clean
	$(MAKE) -C pzflash clean
	$(MAKE) -C bootrom clean
	$(MAKE) -C pztools clean

.PHONY: all toolchain poseidon-headers clean
