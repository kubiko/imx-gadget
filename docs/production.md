# From development to production: U-Boot, OP-TEE and the FDE TA v2

How this gadget builds U-Boot, OP-TEE and the FDE TA for a development board,
what each setting exercises, and what changes for a production device. The
steps any Ubuntu Core A/B device takes, whatever its platform (the model, the
boot gate, the initrd, recovery), are in `docs/production.md` of core-base.

The FDE TA v2 seals keys to a measured boot and retires them with a counter
kept in RPMB. The pkcs11 TA keeps its tokens in the REE FS
(`$SNAP_COMMON/tee-storage`, served by tee-supplicant), whose roll-back
protection can be anchored in RPMB as well. Both need an eMMC whose RPMB
authentication key is programmed. The key can be programmed only once, for
the life of the eMMC.

This gadget is a development build for a board that is not closed, and it
needs no RPMB key. It assumes:

- the board is open: no HAB (i.MX 8M) or AHAB (i.MX 9) enforced, so U-Boot,
  the FIT and the measurements are not trusted, and the HUK is not to be
  relied on (on i.MX 8M the CAAM uses its non-secure master key, so it is
  not secret);
- if an RPMB key is programmed, it is OP-TEE's public test key, so anyone
  with root can forge RPMB writes.

What it exercises depends on the board:

- **No RPMB key** (the default): TA v2's measurement register, seal, unseal,
  reseal and lock, and its fallback without the counter; pkcs11 on the REE
  FS, without roll-back protection. The counter, arming and v1 refusal are
  not exercised.
- **Test key programmed** (optional, below): the counter as well, with no
  rebuild. The REE FS stays without roll-back protection until
  `CFG_REE_FS_INTEGRITY_RPMB` is turned on.

The counter's logic is exercised; it is not secure. A production image
changes the settings below.

## Development build

| Option | Value | Why |
|---|---|---|
| `CFG_RPMB_FS` | `y` | TA v2 counter |
| `CFG_RPMB_ANNOUNCE_PROBE_CAP` | `y` | the kernel routes RPMB, so the initrd needs no tee-supplicant |
| `CFG_RPMB_TESTKEY` | `y` | the key is public, so U-Boot can program it; with no key programmed it changes nothing |
| `CFG_RPMB_WRITE_KEY` | `n` | `y` takes the legacy, tee-supplicant RPMB path, which hangs the first TA call in the initrd |
| `CFG_REE_FS_INTEGRITY_RPMB` | `n` | `y` fails every REE FS open, pkcs11 with it (`init_persistent_db: Failed to create db: 0xffff0008`), while no RPMB key is programmed |
| `CFG_REE_FS_ALLOW_RESET` | `y` | only with integrity on: a reflash wipes `tee-storage`; start a new store then instead of failing |
| `CFG_FDE_V2_REQUIRE_RPMB` | `n` | the TA falls back to sealing without the counter when RPMB is not usable |

### Optional: program the test key, once per board

Only needed to exercise the counter. This build runs without it: TA v2 then
seals without the counter.

This cannot be undone. The eMMC keeps OP-TEE's public test key for good: on
this board it can never take a key derived from the HUK, so do not do this on
a board that will ever be closed for production.

From the U-Boot console:

```
=> mmc list                      # check that emmc_dev is the eMMC
=> mmc dev ${emmc_dev}
=> setexpr a ${loadaddr}
=> mw.l ${a} c33eebd3
=> setexpr a ${a} + 4
=> mw.l ${a} 9f4c336e
=> setexpr a ${a} + 4
=> mw.l ${a} c0e28c98
=> setexpr a ${a} + 4
=> mw.l ${a} 615459b8
=> setexpr a ${a} + 4
=> mw.l ${a} 86cf2b0d
=> setexpr a ${a} + 4
=> mw.l ${a} f24d8464
=> setexpr a ${a} + 4
=> mw.l ${a} c6e656ab
=> setexpr a ${a} + 4
=> mw.l ${a} e401b71b
=> md.b ${loadaddr} 0x20         # d3 eb 3e c3 6e 33 4c 9f ... 1b b7 01 e4
=> mmc rpmb key ${loadaddr}      # answer y
=> mmc rpmb counter              # the key works: prints the write counter
```

Type each line on its own rather than joining them with `;`, so each
`${a}` is expanded after the `setexpr` before it. U-Boot needs `CONFIG_CMD_MMC_RPMB` and
`CONFIG_SUPPORT_EMMC_RPMB`.

These are the bytes of `rpmb_test_key` in `core/tee/tee_rpmb_fs.c` of
OP-TEE. Check them against the OP-TEE version this gadget builds before you
program them.

To also exercise the roll-back protection of the pkcs11 tokens, build with
`CFG_REE_FS_INTEGRITY_RPMB=y` once the key is programmed. The tokens
survive: with no hash in RPMB yet, OP-TEE takes the existing `dirf.db` and
anchors it. `CFG_REE_FS_ALLOW_RESET=y` keeps the board safe to reflash then.

### After programming the key on an existing install

- **FDE keys:** v2 keys sealed before the key was programmed carry no
  counter. They unseal until the first v2 seal or reseal with RPMB arms the
  device. After that they are refused, along with v1 keys. A key that is not
  resealed before arming is lost, so a reinstall is the clean start.
- **The RPMB counter survives reflashing and reinstalls.** An installer has
  to read the counter (`TA_CMD_GET_COUNTER`) and seal at or above it.
  Otherwise the seal is refused ("refusing to seal below the counter").
  Resetting RPMB takes a one-off OP-TEE build with `CFG_RPMB_RESET_FAT=y`.

### What to exercise

Without a key:

- `TA_CMD_GET_COUNTER` reports no RPMB; `TA_CMD_RAISE_COUNTER` fails with
  `TEE_ERROR_STORAGE_NOT_AVAILABLE`.
- A v2 key unseals only while the register is one of its values, and a
  reseal is authorized by a value the register had earlier in the boot.
- Once the TA is locked, decrypts are refused and reseals still work.
- pkcs11 tokens are created and kept across reboots; a reflash starts a new
  store.

With the test key programmed:

- `TA_CMD_GET_COUNTER` reports RPMB present, unarmed before the first v2 seal
  and armed after it.
- Raising the counter retires the handles sealed below it. A seal below the
  counter is refused, and so is lowering the counter.
- Once armed, v1 decrypts and v2 handles sealed without the counter are
  refused.
- With `CFG_REE_FS_INTEGRITY_RPMB=y`, REE FS roll-back: copy `tee-storage`,
  change a pkcs11 token, put the old copy back. The pkcs11 TA fails with
  `TEE_ERROR_SECURITY`. If you delete `tee-storage` instead, a new, empty
  store starts (`CFG_REE_FS_ALLOW_RESET`).
- RPMB tampering: writing a block of the RPMB FS with the test key from
  U-Boot (`mmc rpmb write`) breaks its decryption. The TA then refuses every
  call until the reboot.
- With `CFG_FDE_V2_REQUIRE_RPMB=y` built in, the fail-closed paths of a
  production build run as well: every call that needs the counter fails when
  RPMB does not answer. The key must be programmed first, or the install
  fails.

## Production image

Change the following, in this order of importance.

1. **Close the device before the RPMB key is programmed.** The RPMB key is
   derived from the HUK, and has to come from the HUK of the closed part.
   - i.MX 8M: enforce HAB with the production SRK and set the SNVS to
     secure. The HUK is the CAAM master key verification blob. On an open
     part the CAAM uses its non-secure test master key, so the HUK, and every
     key derived from it, is public, and it changes when the part is closed.
   - i.MX 9: enforce AHAB with the production SRK and move the lifecycle to
       OEM closed. The HUK is derived by the EdgeLock Enclave (OP-TEE
     `core/drivers/crypto/ele/ele.c`). Confirm with NXP whether it changes
     with the lifecycle; until then, treat it as if it does.
   - A key programmed into RPMB from a HUK that later changes no longer
     matches, and RPMB on that eMMC is lost for good. TA keys are derived
     from the HUK too, so keys sealed before it changes do not unseal after.
2. **`CFG_RPMB_TESTKEY=n`.** The RPMB key is then derived from the HUK and the
   eMMC CID. A production device must never run an OP-TEE built with the test
   key. Check `CFG_RPMB_TESTKEY=n` in the built `out/conf.mk` as part of the
   release.
3. **Program the RPMB key in the factory, not in the field.** OP-TEE cannot
   export a key derived from the HUK, so it has to program the key itself.
   Run a one-off provisioning OP-TEE with `CFG_RPMB_WRITE_KEY=y` from a
   factory image in which tee-supplicant is running before anything reaches
   RPMB, for example an image without FDE. The production OP-TEE then keeps
   `CFG_RPMB_WRITE_KEY=n`, so it stays on the kernel RPMB route the initrd
   needs. The provisioning build programs the first RPMB it finds, so it must
   never leave the factory.
   - OP-TEE programs the key only on a closed board on the i.MX 8M (SNVS), and
     on any board on the i.MX 9.
   - The factory flow: fuse and close, boot the provisioning image once, then
     flash the product image and let it install. The provisioning image is never
     flashed to the device; its `uuu` script sits next to the flashing one.
   - An alternative not written: a U-Boot command, proposed as `ucfde provision`,
     that makes the FDE TA reach RPMB from U-Boot, which serves the legacy RPMB
     path itself (`CONFIG_SUPPORT_EMMC_RPMB`), so no factory image is needed.
4. **`CFG_FDE_V2_REQUIRE_RPMB=y`.**
   - Without RPMB, every call that needs the counter fails, a v1 unseal
     included, and v2 keys sealed without the counter are refused.
   - A normal world that blocks or fakes RPMB from the start of the boot can
     no longer downgrade the device to keys without the counter.
   - A device without a programmed key cannot install, which is what
     production wants.
   - v1 keys still unseal until the first v2 seal arms the device, so
     devices upgraded from v1 can move their keys over.
5. **`CFG_REE_FS_INTEGRITY_RPMB=y`.** The development build turns it off so a
   board without an RPMB key keeps pkcs11. In production it anchors the
   pkcs11 tokens in RPMB, so an old copy of `tee-storage` is refused. It
   needs the key from step 3: without one, pkcs11 fails to start.
6. **`CFG_REE_FS_ALLOW_RESET`: decide.**
   - `n`: a missing `tee-storage` with a hash in RPMB fails every REE FS
     open, so a factory reset or reinstall leaves pkcs11 unusable until RPMB
     is reset.
   - `y`: only a missing store is replaced by an empty one; an old store is
     still refused. The cost is that whoever can delete `tee-storage` can
     wipe the tokens, which they can already do by making them unusable.
   - Keep `y` unless deleting the store must be detected.
7. **Production keys in place of `dev-pub-keys`.** `TA_PUBLIC_KEY` (the
   signing key of TAs loaded from the REE FS), the FIT signing key in
   U-Boot, and the UEFI PK/KEK/db all come from the development repository
   today. With `CFG_REE_FS_TA=y`, anyone holding the TA signing key can load
   a TA, so consider `CFG_REE_FS_TA=n` if every TA needed is an early TA (the
   FDE TA and pkcs11 are). The sealed keys do not depend on the TA signing key,
   only on the HUK and the TA's UUID, so replacing it costs no reinstall.
8. **Keep the defaults** `CFG_RPMB_RESET_FAT=n`, `CFG_RPMB_FS_DEBUG_DATA=n`
   and `CFG_TEE_CORE_DEBUG=n`.

9. **U-Boot verifies the FIT, and cannot be interrupted.**
   - The FIT signing key is a production key in U-Boot's device tree, with the
     signature required. U-Boot measures what it boots into TA v2's register; an
     unsigned FIT on a closed board would let anyone boot their own kernel and
     initrd, which can then extend the register with whatever unseals.
   - No interruptible prompt: `bootdelay=-2` or `CONFIG_AUTOBOOT_KEYED`, and no
     console on a closed board, as the `qcm-uc-mini` U-Boot does. At the prompt
     anyone can change `bootargs`, or talk to the TA before Linux does.
   - Nothing to do for the boot gate: `fixup_secure_linux()` appends `panic=-1`
     instead of `dangerous` once the board is closed, and the initrd then
     refuses unasserted essential snaps.
   - U-Boot's `snapd_cmdline` env callback checks the shape of the variables
     snapd writes, and `ucfde args` passes snapd's kernel arguments only with
     TA v2, which measures them (ADR-0059 of core-base).

The register of TA v2 measures what U-Boot boots. It means something only
with steps 1, 7 and 9: a U-Boot that is not verified can feed the TA the
measurements of a boot it did not make.

## The command line, storage and recovery

- **U-Boot's own words of the command line are a contract for the life of a
  device:** the partitions and the console, which TA v2 keys are sealed to as the
  running boot passed them. A U-Boot update that changes them needs a migration
  that reseals first.
- **Set the console verbosity in U-Boot's default env before the first device
  ships.** At 115200 baud the kernel's log and systemd's status lines pace the
  boot, 2.5 to 3.5 s on the i.MX boards; `quiet` takes nearly all of it off, and
  `loglevel=4` about two thirds. As U-Boot's words are sealed to, adding `quiet`
  later takes the migration above; a device can still add it through
  `system.kernel.cmdline-append`, which snapd measures and reseals.
- **TA v2 needs an eMMC boot.** Its counter lives in RPMB, which an SD card does
  not have.
- **There is no recovery key on OP-TEE** (a decision of ADR-0059). A device whose
  boot the keys are not sealed to loses its data, and a factory reset recovers
  the device.
