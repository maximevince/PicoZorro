# Amiga toolchain for everything under amiga/: bebbo's m68k-amigaos-gcc
# 6.5.0b and vasm from a docker image pinned by digest. A local bebbo
# install works too:
#   make AMIGA_GCC=m68k-amigaos-gcc AMIGA_VASM=vasmm68k_mot
# Headers: the NDK 3.2 built into the image; SANA-II/Roadshow and Poseidon
# headers from .toolchain (`make -C amiga toolchain`).
AMIGA_TOP := $(abspath $(dir $(lastword $(MAKEFILE_LIST)))..)
REPO_TOP  := $(abspath $(AMIGA_TOP)/..)
TC        := $(AMIGA_TOP)/.toolchain

BEBBO_IMAGE := sacredbanana/amiga-compiler:m68k-amigaos@sha256:c3f7b87d571a3d8a8e8d49d0ad798a08483d17363adc0488925b28772e0b3a65
DOCKER_RUN  := docker run --rm -u $(shell id -u):$(shell id -g) -v $(REPO_TOP):$(REPO_TOP) -w $(CURDIR) $(BEBBO_IMAGE)
AMIGA_GCC   ?= $(DOCKER_RUN) m68k-amigaos-gcc
AMIGA_VASM  ?= $(DOCKER_RUN) vasmm68k_mot

SANA_INC := $(TC)/NDK3.2/SANA+RoadshowTCP-IP/include
NET_INC  := $(TC)/NDK3.2/SANA+RoadshowTCP-IP/netinclude
PSD_INC  := $(TC)/poseidon/include

# Programs (tests, pzflash): libnix startup, plain 68000.
PROG_CFLAGS  := -m68000 -noixemul -std=gnu99 -Os -fomit-frame-pointer -Wall -Wno-pointer-sign -Wno-format
PROG_LDFLAGS := -m68000 -noixemul -s

# Drivers: no startup code or C library, -mregparm=2 and LTO.
# Everything whose address leaves a driver carries REGARG registers
# (common/compiler.h). romtag.c is compiled with DEV_CFLAGS_ROMTAG (no LTO),
# which keeps LibNull at the start of the code hunk.
DEV_CFLAGS        := $(PROG_CFLAGS) -flto -mregparm=2
DEV_CFLAGS_ROMTAG := $(PROG_CFLAGS) -mregparm=2
DEV_LDFLAGS       := -m68000 -noixemul -Os -flto -mregparm=2 -nostdlib -nostartfiles -s
# libgcc lacks the 68000 multiply/divide helpers in this toolchain; libnix13
# has them (__mulsi3, __udivsi3, __umodsi3).
DEV_LIBS          := -lgcc -lnix13
