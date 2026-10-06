# PicoZorro top-level build.
#   make                     host tests + RP2350 firmware
#   make fw                  firmware, every binary (firmware/)
#   make fw-test             host unit tests of pz-core
#   make boot                the boot image the firmware embeds: boot ROM + the Amiga drivers
#   make amiga               Amiga drivers, pzflash, tools (bebbo GCC in docker; once: make -C amiga toolchain)
#   make fw-uf2              picozorro as firmware/picozorro-install.uf2: drop it on the module in
#                            BOOTSEL mode, it writes the partition table and the image in A (docs/UPDATE.md)
#   make fw-uf2 BIN=hello    a plain UF2 of any binary, firmware/hello.uf2
#   make fw-install          picozorro into the A/B partitions over SWD (probe-rs)
#   make fw-pzf              picozorro as an update file, firmware/picozorro.pzf (docs/UPDATE.md)
#   make fw-run BIN=hello    flash a binary over SWD and stream its defmt log
#   make fw-load BIN=hello   flash a binary over USB (picotool), no probe
#   make fw-clippy           clippy on pz-app, default features and the LOG=1 set
#   make check-one-th        ERC + DRC of the PicoZorro One TH board
# The default features build the PicoZorro One's image. FEATURES=... adds to
# them; naming nic-enc28j60 or usb-log in FEATURES replaces them
# (--no-default-features). LOG=1 is the One's image with the USB CDC log
# instead of the USB host (FEATURES=$(LOG_FEATURES)).
CARGO := cargo
BIN ?= picozorro
LOG_FEATURES := nic-w5500,mpeg,oc240,xrdy-direct,int-nfet,usb-log
ifneq ($(LOG),)
FEATURES := $(LOG_FEATURES)
endif
FEATURE_ARGS = $(if $(findstring nic-enc28j60,$(FEATURES))$(findstring usb-log,$(FEATURES)),--no-default-features) \
	$(if $(FEATURES),--features $(FEATURES))
FW_OUT := firmware/target/thumbv8m.main-none-eabihf/release
PZ_ELF := $(FW_OUT)/picozorro

.PHONY: all fw fw-test fw-run fw-load fw-pzf fw-uf2 fw-install fw-clippy boot amiga check-one-th clean
all: fw-test fw

fw:
	cd firmware && $(CARGO) build --release

fw-test:
	cd firmware && $(CARGO) test-host

fw-run:
	cd firmware && $(CARGO) run --release --bin $(BIN) $(FEATURE_ARGS)

fw-load:
	cd firmware && $(CARGO) build --release --bin $(BIN) $(FEATURE_ARGS)
	picotool load -f -x -t elf $(FW_OUT)/$(BIN)

fw-pzf:
	cd firmware && $(CARGO) build --release --bin picozorro $(FEATURE_ARGS)
	python3 tools/mkpzf.py $(PZ_ELF) firmware/picozorro.pzf

# picozorro: the install UF2 (partition table, image in A, B cleared);
# any other binary: a plain rp2350-arm-s UF2.
fw-uf2:
	cd firmware && $(CARGO) build --release --bin $(BIN) $(FEATURE_ARGS)
ifeq ($(BIN),picozorro)
	picotool partition create firmware/pt.json $(FW_OUT)/pt.bin
	python3 tools/mkpzf.py --uf2 $(FW_OUT)/pt.bin $(PZ_ELF) firmware/picozorro-install.uf2
else
	picotool uf2 convert $(FW_OUT)/$(BIN) -t elf firmware/$(BIN).uf2 -t uf2 --family rp2350-arm-s
endif

fw-install:
	cd firmware && $(CARGO) build --release --bin picozorro $(FEATURE_ARGS)
	tools/pz_install.sh $(PZ_ELF)

fw-clippy:
	cd firmware && $(CARGO) clippy --release -p pz-app
	cd firmware && $(CARGO) clippy --release -p pz-app --no-default-features --features $(LOG_FEATURES)

# The boot image: the boot ROM plus picozorro.device, picozorrousb.device and
# mpega.library, which the firmware serves to Kickstart so the Amiga needs no
# files on disk. Without firmware/boot.img the firmware builds with no boot ROM.
BOOT_MODULES := amiga/picozorro.device/picozorro.device amiga/picozorrousb.device/picozorrousb.device \
	amiga/mpega.library/mpega.library
boot:
	$(MAKE) -C amiga/picozorro.device picozorro.device
	$(MAKE) -C amiga/picozorrousb.device picozorrousb.device
	$(MAKE) -C amiga/mpega.library mpega.library
	$(MAKE) -C amiga/bootrom
	python3 tools/mkboot.py -o firmware/boot.img amiga/bootrom/boot.bin $(BOOT_MODULES)

amiga:
	$(MAKE) -C amiga

ONETH := hardware/picozorro-one-th
check-one-th:
	@mkdir -p $(ONETH)/out
	cd $(ONETH) && kicad-cli sch erc --severity-all --exit-code-violations -o out/erc.rpt picozorro-one-th.kicad_sch
	cd $(ONETH) && kicad-cli pcb drc --refill-zones --severity-all --schematic-parity \
	    --exit-code-violations -o out/drc.rpt picozorro-one-th.kicad_pcb

clean:
	rm -rf firmware/target $(ONETH)/out
	-$(MAKE) -C amiga clean
